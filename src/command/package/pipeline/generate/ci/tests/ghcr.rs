// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── GHCR target: login path + package write permission (E-P4) ──────────

const GHCR_SPEC: &str = r#"
name: bazelisk
target:
  registry: ghcr.io
  repository: ocx-contrib/bazelbuild/bazelisk
source:
  type: github_release
  owner: bazelbuild
  repo: bazelisk
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "bazelisk-linux-amd64$"
asset_type:
  type: binary
  name: bazelisk
tests:
  - name: version
    command: bazelisk --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
announce:
  package: bazelbuild/bazelisk
  fork: ocx-contrib/index
"#;

#[test]
fn ghcr_push_job_grants_every_scope_its_steps_need() {
    // Declaring ANY permission sets every unnamed scope to `none`, so this
    // block is the whole token for the push job and a missing line is a
    // revoked capability, not a default. Asserted as one exact block —
    // checking only `packages: write` would have passed against the version
    // that silently revoked the other three.
    //
    // Per step:
    //   contents: read       — actions/checkout, setup-ocx
    //   packages: write      — docker login ghcr.io + `ocx package push`
    //   actions: read        — `gh api …/runs/N/jobs` for OCX_MIRROR_JOB_URL,
    //                          whose `|| true` swallows a 403 and silently
    //                          drops the link from every Discord row
    //   checks: write        — publish-unit-test-result-action's check run
    //   pull-requests: write — the same action's PR comment; without the
    //                          pair it 403s under `if: always()` and reds
    //                          the job on a perfectly successful publish
    // download-artifact / upload-artifact use the runtime token for
    // same-run artifacts and need no scope here; the announce authenticates
    // with OCX_ANNOUNCE_TOKEN, not with GITHUB_TOKEN.
    let workflow = workflow_of(GHCR_SPEC);

    assert!(
        workflow.contains(
            "    permissions:\n\
             \x20     contents: read\n\
             \x20     packages: write\n\
             \x20     actions: read\n\
             \x20     checks: write\n\
             \x20     pull-requests: write\n"
        ),
        "ghcr.io push job must grant exactly the scopes its steps need, got:\n{workflow}"
    );
}

#[test]
fn ghcr_target_logs_in_with_github_token() {
    let workflow = workflow_of(GHCR_SPEC);

    assert!(
        workflow.contains("      packages: write\n"),
        "a ghcr.io push job must declare packages: write, got:\n{workflow}"
    );
    assert!(
        workflow.contains("docker login ghcr.io"),
        "ghcr.io target must log in to ghcr.io, got:\n{workflow}"
    );
    assert!(
        workflow.contains(r#"-u "${{ github.actor }}""#),
        "ghcr.io login must use the run's own actor, got:\n{workflow}"
    );
    // The shared OCX_MIRROR_REGISTRY_* org secrets carry ocx.sh credentials
    // used by every other mirror repo. Repurposing them for GHCR would
    // break all of them.
    assert!(
        !workflow.contains("OCX_MIRROR_REGISTRY_TOKEN"),
        "ghcr.io path must not touch the shared ocx.sh registry secrets, got:\n{workflow}"
    );
    assert!(
        !workflow.contains("OCX_MIRROR_REGISTRY_USER"),
        "ghcr.io path must not touch the shared ocx.sh registry secrets, got:\n{workflow}"
    );
}

#[test]
fn ghcr_target_is_always_credentialed_so_the_push_never_silently_skips() {
    // GITHUB_TOKEN is present on every run. If the probe kept testing for
    // OCX_MIRROR_REGISTRY_TOKEN, every GHCR push would take the "no creds"
    // branch and skip while still reporting success.
    let workflow = workflow_of(GHCR_SPEC);

    assert!(
        workflow.contains("run: echo \"have=true\" >> \"${GITHUB_OUTPUT}\"\n"),
        "ghcr.io credential probe must be a constant have=true, got:\n{workflow}"
    );
    assert!(
        !workflow.contains("have=false"),
        "ghcr.io workflow must have no no-credentials branch, got:\n{workflow}"
    );
}

/// The `discover:` job only, so a push-job step cannot satisfy an
/// assertion about discover.
fn discover_job(workflow: &str) -> String {
    let start = workflow.find("\n  discover:").expect("workflow has a discover job");
    let rest = &workflow[start + 1..];
    let end = rest.find("\n  prepare:").expect("workflow has a prepare job");
    rest[..end].to_string()
}

#[test]
fn ghcr_discover_authenticates_so_a_first_publish_can_bootstrap() {
    // ghcr.io answers an anonymous read of a missing repository with 403
    // DENIED, not 404. `list_target_tags` only treats an authoritative
    // not-found as an empty target (issue #157), so an unauthenticated
    // discover aborts the run before the push that would create the
    // package — the target could never come into existence.
    let discover = discover_job(&workflow_of(GHCR_SPEC));

    assert!(
        discover.contains("docker login ghcr.io"),
        "a ghcr.io discover job must log in, got:\n{discover}"
    );
    assert!(
        discover.contains("      packages: read\n"),
        "a ghcr.io discover job must grant packages: read, got:\n{discover}"
    );
    assert!(
        discover.contains("      contents: read\n"),
        "naming any permission zeroes the rest — checkout still needs contents: read, got:\n{discover}"
    );
    // Discover reads; only the push job writes.
    assert!(
        !discover.contains("packages: write"),
        "discover must not ask for write access, got:\n{discover}"
    );
}

#[test]
fn non_ghcr_discover_stays_anonymous_and_unprivileged() {
    // A public ocx.sh target lists tags anonymously, and the shared
    // OCX_MIRROR_REGISTRY_* secrets stay confined to the push job.
    let discover = discover_job(&workflow_of(SHFMT_SPEC));

    assert!(
        !discover.contains("docker login"),
        "a non-GHCR discover job must not log in, got:\n{discover}"
    );
    assert!(
        !discover.contains("permissions:"),
        "a non-GHCR discover job keeps the repository default scopes, got:\n{discover}"
    );
}

#[test]
fn ghcr_describe_logs_in_with_the_run_token_not_the_ocx_sh_secrets() {
    // `describe` pushes the catalog metadata as an `__ocx.desc` referrer on
    // the target. Switching the target host to ghcr.io without switching
    // the credential left it feeding ocx.sh org-secret credentials to
    // ghcr.io — a login that cannot succeed, and a scope the job never got.
    let describe = describe_of(GHCR_SPEC);

    assert!(
        describe.contains("docker login ghcr.io"),
        "a ghcr.io describe job must log in to ghcr.io, got:\n{describe}"
    );
    assert!(
        describe.contains(r#"-u "${{ github.actor }}""#),
        "a ghcr.io describe job must use the run's own actor, got:\n{describe}"
    );
    assert!(
        !describe.contains("secrets.OCX_MIRROR_REGISTRY_"),
        "the ocx.sh org secrets must not reach a ghcr.io describe job, got:\n{describe}"
    );
    assert!(
        describe.contains("      packages: write\n") && describe.contains("      contents: read\n"),
        "a ghcr.io describe job writes a referrer, so it needs packages: write plus contents: read for checkout, got:\n{describe}"
    );
}

#[test]
fn non_ghcr_describe_keeps_the_org_secret_login_and_adds_no_permissions() {
    let describe = describe_of(SHFMT_SPEC);

    assert!(
        describe.contains("docker login ocx.sh"),
        "an ocx.sh describe job keeps its own login, got:\n{describe}"
    );
    assert!(
        describe.contains(r#"-u "${{ secrets.OCX_MIRROR_REGISTRY_USER }}""#),
        "an ocx.sh describe job keeps the org-secret credentials, got:\n{describe}"
    );
    assert!(
        !describe.contains("permissions:"),
        "a non-GHCR describe job keeps the repository default scopes, got:\n{describe}"
    );
}

#[test]
fn non_ghcr_target_keeps_the_registry_secret_login_and_adds_no_permissions() {
    let workflow = workflow_of(SHFMT_SPEC);

    assert!(
        workflow.contains("docker login ocx.sh"),
        "ocx.sh target must keep its own login, got:\n{workflow}"
    );
    assert!(
        workflow.contains(r#"-u "${{ secrets.OCX_MIRROR_REGISTRY_USER }}""#),
        "ocx.sh target must keep the org-secret credentials, got:\n{workflow}"
    );
    assert!(
        !workflow.contains("packages: write"),
        "a non-GHCR push job needs no extra token scope, got:\n{workflow}"
    );
    assert!(
        !workflow.contains("docker login ghcr.io"),
        "ocx.sh target must not log in to ghcr.io, got:\n{workflow}"
    );
}

#[test]
fn push_step_carries_the_announce_token() {
    // The announce happens inside `ocx-mirror package pipeline push`, so
    // the token has to reach that step's env — there is no separate job.
    for spec in [SHFMT_SPEC, GHCR_SPEC] {
        let workflow = workflow_of(spec);
        assert!(
            workflow.contains("OCX_ANNOUNCE_TOKEN: ${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
            "push step must carry OCX_ANNOUNCE_TOKEN, got:\n{workflow}"
        );
    }
}

#[test]
fn every_placeholder_is_substituted_for_both_registries() {
    for spec in [SHFMT_SPEC, GHCR_SPEC] {
        let workflow = workflow_of(spec);
        assert!(
            !workflow.contains("{PUSH_PERMISSIONS}") && !workflow.contains("{REGISTRY_AUTH_STEPS}"),
            "unsubstituted placeholder in:\n{workflow}"
        );
    }
}

#[test]
fn a_cross_owner_ghcr_target_is_warned_about_at_generate_time() {
    // GITHUB_TOKEN authorises packages under its own repository's owner.
    // `docker login ghcr.io` succeeds regardless — login does not
    // authorise — and the GHCR credential probe is a constant `have=true`,
    // so a cross-owner target has no honest skip: the push just reds with
    // `denied: installation not allowed to Create organization package`.
    let spec = spec_from_yaml(GHCR_SPEC); // target owner: ocx-contrib
    assert!(ghcr_owner_warning(&spec, Some("ocx-contrib/mirror-bazelisk")).is_none());
    assert!(
        ghcr_owner_warning(&spec, Some("OCX-Contrib/mirror-bazelisk")).is_none(),
        "GHCR owners are case-insensitive"
    );
    assert!(
        ghcr_owner_warning(&spec, None).is_none(),
        "generate cannot always know the remote — unknown owner must stay quiet"
    );
    assert!(
        ghcr_owner_warning(&spec_from_yaml(SHFMT_SPEC), Some("someone-else/x")).is_none(),
        "a non-GHCR target authenticates with an org secret, not with repo ownership"
    );

    let warning =
        ghcr_owner_warning(&spec, Some("someone-else/mirror-bazelisk")).expect("cross-owner target must warn");
    assert!(warning.contains("ocx-contrib"), "got: {warning}");
    assert!(warning.contains("someone-else"), "got: {warning}");
}

#[test]
fn the_run_summary_artifact_carries_the_announce_tags_file() {
    // The tags file is the exact `--tags-from-file` the index call received.
    // Uploading only run-summary.json leaves nothing to reconstruct a
    // failed announce from.
    let workflow = workflow_of(GHCR_SPEC);

    assert!(
        workflow.contains("          path: |\n            run-summary.json\n            run-summary.announce-tags\n"),
        "the run-summary artifact must carry the announce tags file, got:\n{workflow}"
    );
}

#[test]
fn ghcr_announce_fixture_renders_end_to_end() {
    // The inline specs above bypass `load_spec`. This one goes through it,
    // so the fixture also proves the `announce:` block survives
    // deny_unknown_fields and spec validation on the way to a written file.
    let dir = tempdir().unwrap();
    render_fixture("mirror-ghcr-announce.yml", dir.path()).expect("ghcr + announce fixture must render");

    let content = std::fs::read_to_string(dir.path().join(".github/workflows/mirror.yml")).unwrap();
    assert!(content.contains("packages: write"), "got:\n{content}");
    assert!(content.contains("docker login ghcr.io"), "got:\n{content}");
    assert!(
        content.contains("OCX_ANNOUNCE_TOKEN: ${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
        "got:\n{content}"
    );
}
