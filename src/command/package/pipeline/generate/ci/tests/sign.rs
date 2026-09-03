// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `sign:` — what a signing identity changes in a rendered workflow.
//!
//! Two contracts. C-071: a keyless spec adds `id-token: write` to the push
//! and patch jobs, inside the one `permissions:` block each already has (or
//! opens for the purpose), and every `env://NAME` the spec names reaches the
//! signing step as a `secrets.`/`vars.` mapping. C-072: no signing step is
//! reachable from a `pull_request` trigger without the fork gate.

use super::super::*;
use super::support::*;

/// A non-GHCR spec with `{SIGN}` spliced in as its `sign:` block, or with no
/// `sign:` at all when the argument is empty.
fn spec_yaml(sign: &str) -> String {
    format!(
        r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
{sign}"#
    )
}

/// The same shape against `ghcr.io`, whose push job already carries a
/// `permissions:` block.
fn ghcr_spec_yaml(sign: &str) -> String {
    spec_yaml(sign).replace("registry: ocx.sh", "registry: ghcr.io")
}

const KEYLESS: &str = "sign:\n  keyless: {}\n";
const KEYLESS_ENDPOINTS: &str = "sign:\n  keyless:\n    fulcio: env://SIGSTORE_FULCIO_URL\n    rekor: env://SIGSTORE_REKOR_URL\n    identity_token: env://SIGSTORE_ID_TOKEN\n";
const KEY_STRING: &str = "sign:\n  key: env://MIRROR_SIGNING_KEY\n";
const KEY_FULL: &str = "sign:\n  key:\n    ref: file:///run/secrets/mirror.key\n    passphrase: env://MIRROR_KEY_PASSPHRASE\n    rekor: env://SIGSTORE_REKOR_URL\n";

/// The `permissions:` mapping of `job` in a rendered workflow, or `None` when
/// the job declares none and keeps the repository's default token scopes.
fn permissions_of(rendered: &str, job: &str) -> Option<serde_yaml_ng::Value> {
    let parsed: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(rendered).unwrap_or_else(|e| panic!("rendered workflow must parse: {e}\n{rendered}"));
    parsed["jobs"][job]
        .get("permissions")
        .filter(|value| !value.is_null())
        .cloned()
}

/// Every `permissions:` key a rendered workflow declares, as
/// `(job, occurrences)`.
///
/// Counted off the raw text rather than the parsed document on purpose: YAML
/// keeps the last of two duplicate keys silently, so a renderer that opened a
/// second `permissions:` block beside the first would parse clean and lose the
/// first block's scopes at runtime.
fn permission_block_counts(rendered: &str) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for line in rendered.lines() {
        if let Some(name) = line.strip_prefix("  ").and_then(|rest| rest.strip_suffix(':'))
            && !name.starts_with(' ')
            && !name.starts_with('#')
        {
            counts.push((name.to_string(), 0));
        }
        if line == "    permissions:"
            && let Some(last) = counts.last_mut()
        {
            last.1 += 1;
        }
    }
    counts
}

#[test]
fn a_keyless_spec_opens_a_push_permissions_block_on_a_non_ghcr_target() {
    // S-050's blast radius: a non-GHCR push job has never declared any
    // permission, so it runs on the repository's default token scopes.
    // Naming `id-token: write` sets every unnamed scope to `none`, so the
    // block has to pay for the job's other steps at the same time — checkout,
    // the `gh api` job-URL lookup, and publish-unit-test-result-action's
    // check run and PR comment. Asserting only `id-token` would pass against
    // the version that silently revoked the other four.
    let permissions = permissions_of(&workflow_of(&spec_yaml(KEYLESS)), "push")
        .expect("a keyless spec must declare push permissions on a non-GHCR target");
    let expected: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        "contents: read\nactions: read\nchecks: write\npull-requests: write\nid-token: write\n",
    )
    .unwrap();
    assert_eq!(permissions, expected, "push permissions for a keyless non-GHCR spec");
}

#[test]
fn a_keyless_spec_adds_id_token_to_the_ghcr_push_block() {
    // The GHCR cell: the block already exists for `packages: write`, so
    // `id-token` joins it and every scope the job already had survives.
    let permissions = permissions_of(&workflow_of(&ghcr_spec_yaml(KEYLESS)), "push")
        .expect("a GHCR push job always declares permissions");
    let expected: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        "contents: read\npackages: write\nactions: read\nchecks: write\npull-requests: write\nid-token: write\n",
    )
    .unwrap();
    assert_eq!(permissions, expected, "push permissions for a keyless GHCR spec");
}

#[test]
fn a_key_mode_spec_renders_no_id_token() {
    // Key mode signs with material the spec names; it never exchanges an
    // OIDC token, so granting `id-token: write` would hand the job a
    // capability nothing in it uses. Both key forms, both registries.
    for sign in [KEY_STRING, KEY_FULL] {
        for rendered in [workflow_of(&spec_yaml(sign)), workflow_of(&ghcr_spec_yaml(sign))] {
            assert!(
                !rendered.contains("id-token"),
                "a key-mode spec must not grant id-token; rendered:\n{rendered}"
            );
        }
    }
    // The negative above can only fail if the positive is reachable at all.
    assert!(
        workflow_of(&spec_yaml(KEYLESS)).contains("id-token: write"),
        "the id-token needle must match for a keyless spec, or the assertions above are vacuous"
    );
}

#[test]
fn an_unsigned_spec_renders_exactly_what_it_rendered_before() {
    // The regression that matters most: `sign:` is opt-in, and ~40 pinned
    // mirror repositories have not opted in. Their bytes must not move.
    // `tests/golden/` is the byte-exact half of this; here is the reason.
    let unsigned = workflow_of(&spec_yaml(""));
    assert!(
        permissions_of(&unsigned, "push").is_none(),
        "an unsigned non-GHCR push job declares no permissions:\n{unsigned}"
    );
    assert!(!unsigned.contains("id-token"), "an unsigned spec grants no id-token");
    assert!(
        !unsigned.contains("secrets.SIGSTORE") && !unsigned.contains("vars."),
        "an unsigned spec maps no signing variables"
    );
}

#[test]
fn the_discover_job_never_gains_id_token() {
    // Discover lists the target's tags and writes nothing — it never pushes,
    // so it never signs. Its `permissions:` block is the read-only one, and
    // a signing identity must not widen it.
    for sign in [KEYLESS, KEYLESS_ENDPOINTS, KEY_STRING, KEY_FULL] {
        for yaml in [spec_yaml(sign), ghcr_spec_yaml(sign)] {
            let rendered = workflow_of(&yaml);
            let discover = permissions_of(&rendered, "discover");
            assert!(
                discover.is_none_or(|value| value.get("id-token").is_none()),
                "discover must never gain id-token; rendered:\n{rendered}"
            );
        }
    }
}

#[test]
fn no_job_declares_two_permissions_blocks() {
    // C-071 says `id-token: write` lands *inside the single existing block*.
    // Emitting a second one parses fine and silently drops the first block's
    // scopes, which on a GHCR target is `packages: write` — the push would
    // then fail with `denied` on a mirror that had been publishing for years.
    for sign in ["", KEYLESS, KEYLESS_ENDPOINTS, KEY_STRING, KEY_FULL] {
        for yaml in [spec_yaml(sign), ghcr_spec_yaml(sign)] {
            let rendered = workflow_of(&yaml);
            for (job, blocks) in permission_block_counts(&rendered) {
                assert!(
                    blocks <= 1,
                    "job `{job}` declares {blocks} permissions: blocks\n{rendered}"
                );
            }
        }
    }
    // The counter has to be able to see a block at all, or every assertion
    // above is a green count of zero.
    let ghcr = permission_block_counts(&workflow_of(&ghcr_spec_yaml(KEYLESS)));
    assert!(
        ghcr.iter().any(|(job, blocks)| job == "push" && *blocks == 1),
        "the block counter must find the GHCR push block: {ghcr:?}"
    );
}

#[test]
fn env_refs_reach_the_push_step_as_secrets_or_vars() {
    // WP 2's `resolve_sign` reads every `env://NAME` out of the child's own
    // environment, so the workflow is what has to put it there. Secret-class
    // refs (the key, its passphrase, an explicit identity token) come from
    // `secrets.`; the two endpoint URLs are not secrets and come from
    // `vars.`, which is what lets an operator see them in the run log.
    let keyless = workflow_of(&spec_yaml(KEYLESS_ENDPOINTS));
    for (name, scope) in [
        ("SIGSTORE_FULCIO_URL", "vars"),
        ("SIGSTORE_REKOR_URL", "vars"),
        ("SIGSTORE_ID_TOKEN", "secrets"),
    ] {
        let expected = format!("{name}: ${{{{ {scope}.{name} }}}}");
        assert!(
            keyless.contains(&expected),
            "expected `{expected}`; rendered:\n{keyless}"
        );
    }

    let key = workflow_of(&spec_yaml(KEY_FULL));
    for (name, scope) in [("MIRROR_KEY_PASSPHRASE", "secrets"), ("SIGSTORE_REKOR_URL", "vars")] {
        let expected = format!("{name}: ${{{{ {scope}.{name} }}}}");
        assert!(key.contains(&expected), "expected `{expected}`; rendered:\n{key}");
    }
    // `ref: file://…` names a path on the runner, not a variable — nothing to map.
    assert!(
        !key.contains("run/secrets/mirror.key"),
        "a file:// ref is read by the pipeline, never inlined into the workflow:\n{key}"
    );

    // The string form's key ref is itself a secret.
    assert!(
        workflow_of(&spec_yaml(KEY_STRING)).contains("MIRROR_SIGNING_KEY: ${{ secrets.MIRROR_SIGNING_KEY }}"),
        "`key: env://NAME` maps NAME from secrets"
    );
}

#[test]
fn the_patch_job_carries_the_same_signing_surface_as_push() {
    // `patch` re-emits published manifests, so WP 2 signs there too — the
    // scopes and the variable mapping have to follow, or a patched manifest
    // silently loses the signature its original push had.
    let rendered = render_patch(&spec_from_yaml(&spec_yaml(KEYLESS_ENDPOINTS)), &root_slot());
    let permissions =
        permissions_of(&rendered, "patch").expect("a keyless spec must declare patch permissions on a non-GHCR target");
    let expected: serde_yaml_ng::Value = serde_yaml_ng::from_str("contents: read\nid-token: write\n").unwrap();
    assert_eq!(permissions, expected, "patch permissions for a keyless non-GHCR spec");
    assert!(
        rendered.contains("SIGSTORE_FULCIO_URL: ${{ vars.SIGSTORE_FULCIO_URL }}"),
        "patch maps the endpoint variables too:\n{rendered}"
    );

    let ghcr = render_patch(&spec_from_yaml(&ghcr_spec_yaml(KEYLESS)), &root_slot());
    let ghcr_permissions = permissions_of(&ghcr, "patch").expect("a GHCR patch job always declares permissions");
    let ghcr_expected: serde_yaml_ng::Value =
        serde_yaml_ng::from_str("contents: read\npackages: write\nid-token: write\n").unwrap();
    assert_eq!(
        ghcr_permissions, ghcr_expected,
        "patch permissions for a keyless GHCR spec"
    );

    // Unsigned patch is unchanged.
    assert!(permissions_of(&render_patch(&spec_from_yaml(&spec_yaml("")), &root_slot()), "patch").is_none());
}

#[test]
fn describe_and_cascade_gain_nothing_from_a_signing_identity() {
    // C-071 names the push and patch jobs. `describe` publishes catalog
    // metadata and `cascade` re-points rolling tags; neither pushes a package
    // manifest, so neither signs and neither may take the OIDC scope.
    // Both registries: the non-GHCR case renders no block at all, so it
    // cannot fail on a change to the block GHCR does render — which is the
    // one place an `id-token` line could be added by mistake.
    for yaml in [spec_yaml(KEYLESS), ghcr_spec_yaml(KEYLESS)] {
        let spec = spec_from_yaml(&yaml);
        for rendered in [
            render_describe(&spec, &root_slot()),
            render_cascade(&spec, &root_slot()),
        ] {
            assert!(
                !rendered.contains("id-token"),
                "only push and patch sign; rendered:\n{rendered}"
            );
        }
    }
}

#[test]
fn signing_steps_reachable_from_pull_request_carry_the_fork_gate() {
    // C-072. No generated workflow triggers on `pull_request` today — the
    // publish pipeline runs on `push: [main]`, a schedule and a dispatch —
    // so the gate renders nowhere and this test passes by the antecedent
    // being false. That is the point: it asserts the *invariant*, not the
    // current emptiness, so the day a `pull_request` trigger is added to a
    // workflow that signs, this goes red instead of a fork PR reaching for
    // an OIDC token it will not be granted.
    //
    // Proved red by adding `pull_request:` to `templates/workflow.yml`'s
    // `on:` block: this fails naming the `Push` step, and nothing else does.
    for sign in [KEYLESS, KEYLESS_ENDPOINTS, KEY_STRING, KEY_FULL] {
        for yaml in [spec_yaml(sign), ghcr_spec_yaml(sign)] {
            let spec = spec_from_yaml(&yaml);
            for rendered in [
                workflow_of(&yaml),
                render_patch(&spec, &root_slot()),
                render_describe(&spec, &root_slot()),
                render_cascade(&spec, &root_slot()),
                render_announce_from_registry(&spec, &root_slot()),
            ] {
                for step in signing_steps_on_a_pull_request_trigger(&rendered) {
                    assert!(
                        step.contains("github.event_name != 'pull_request'"),
                        "a signing step reachable from a pull_request trigger needs the fork gate:\n{step}"
                    );
                }
            }
        }
    }
    // The finder must be able to name a signing step, or the loop above is a
    // green iteration over nothing for the wrong reason.
    let with_trigger =
        workflow_of(&spec_yaml(KEYLESS)).replace("  push:\n    branches:", "  pull_request:\n  push:\n    branches:");
    assert_eq!(
        signing_steps_on_a_pull_request_trigger(&with_trigger).len(),
        1,
        "the finder must see exactly the Push step once a pull_request trigger exists:\n{with_trigger}"
    );
}

/// The steps of `rendered` that invoke a signing pipeline verb, but only when
/// the workflow can be reached from a `pull_request` trigger; empty otherwise.
///
/// A "signing step" is one that runs `pipeline push` or `pipeline patch` —
/// the two verbs WP 2 gives a `--sign` tail. Matched on the whole invocation,
/// not on the verb: `pipeline patch` alone also appears inside a comment in
/// discover's plan step, which signs nothing. Split on the step bullet so each
/// returned string carries its own `if:`.
fn signing_steps_on_a_pull_request_trigger(rendered: &str) -> Vec<String> {
    let triggers: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(rendered).unwrap_or_else(|e| panic!("rendered workflow must parse: {e}\n{rendered}"));
    if triggers["on"].get("pull_request").is_none() {
        return Vec::new();
    }
    rendered
        .split("\n      - ")
        .filter(|step| {
            step.contains("ocx-mirror package pipeline push") || step.contains("ocx-mirror package pipeline patch")
        })
        .map(str::to_string)
        .collect()
}
