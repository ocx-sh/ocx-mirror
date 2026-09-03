// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Job permissions, registry login steps, and the signing environment.
//!
//! A mirror publishing to `ghcr.io` authenticates with the workflow's own
//! `GITHUB_TOKEN` and needs `packages:` scopes the default token does not
//! carry; every other registry uses explicit credentials and needs neither.
//!
//! A `sign:` block adds a second reason for a job to declare permissions —
//! keyless signing exchanges an OIDC token, which needs `id-token: write` —
//! and a set of `env:` lines carrying the variables the spec names. Both live
//! here because both answer the same question: what capability does this job's
//! credential have to grant.

use std::collections::BTreeMap;

use crate::spec::{KeyConfig, MirrorSpec, Ref};

/// GitHub's own container registry — authenticated with the workflow's
/// `GITHUB_TOKEN`, not with the shared `OCX_MIRROR_REGISTRY_*` org secrets.
pub const GHCR_REGISTRY: &str = "ghcr.io";

/// `permissions:` block for the push job.
///
/// GHCR needs `packages: write` on the run's `GITHUB_TOKEN` to accept a push.
/// Other registries authenticate with an org secret and need no extra token
/// scope, so the block is omitted entirely there and the job keeps the
/// repository's default token scopes.
///
/// Naming *any* permission sets every unnamed scope to `none`, so this block is
/// the whole token for that job and every step in it has to be paid for:
///
/// | Scope | Step that needs it |
/// |---|---|
/// | `contents: read` | `actions/checkout`, `setup-ocx` |
/// | `packages: write` | `docker login ghcr.io` + `ocx package push` |
/// | `actions: read` | `gh api …/actions/runs/N/jobs` resolving the push job URL |
/// | `checks: write` | `publish-unit-test-result-action`'s check run |
/// | `pull-requests: write` | the same action's pull-request comment |
///
/// `actions: read` is the one that fails silently: the `gh api` call ends in
/// `| head -n1 || true`, so a 403 leaves `OCX_MIRROR_JOB_URL` empty and every
/// Discord row quietly loses its link. The two `publish-unit-test-result-action`
/// scopes are the pairing this repository's own `verify.yml` already uses with
/// the same pinned action; without them that step 403s under `if: always()` and
/// reds the push job on every run that published perfectly.
///
/// `actions/upload-artifact` and `actions/download-artifact` authenticate with
/// the runtime token for same-run artifacts, not with `GITHUB_TOKEN`, so they
/// need no scope of their own. The announce subprocess uses `OCX_ANNOUNCE_TOKEN`
/// — a separate secret, not this token.
pub const GHCR_PUSH_PERMISSIONS: &str = "    permissions:\n      contents: read\n      packages: write\n      actions: read\n      checks: write\n      pull-requests: write\n";

/// `id-token: write` — the OIDC scope a keyless signature is exchanged for.
///
/// Always appended to whichever block the job already declares rather than
/// emitted as one of its own: two `permissions:` keys in one job parse as
/// YAML and the later silently wins, which on a GHCR push would drop
/// `packages: write` from a mirror that had been publishing for years.
const ID_TOKEN_PERMISSION: &str = "      id-token: write\n";

/// `permissions:` block for a *non-GHCR* push job that signs keylessly.
///
/// A non-GHCR push job has never declared a permission — it runs on the
/// repository's default token scopes, and that is the first block ever
/// emitted there. Naming `id-token: write` sets every unnamed scope to
/// `none`, so the block has to pay for the job's other steps at the same
/// time: it is [`GHCR_PUSH_PERMISSIONS`] minus `packages: write`, the one
/// scope only ghcr.io's own push needs (every other registry authenticates
/// with `OCX_MIRROR_REGISTRY_TOKEN`). See that constant for the per-step
/// justification of the remaining four.
const SIGNING_PUSH_PERMISSIONS: &str =
    "    permissions:\n      contents: read\n      actions: read\n      checks: write\n      pull-requests: write\n";

/// `permissions:` block for a *non-GHCR* patch job that signs keylessly.
///
/// Patch checks out and installs ocx and nothing else — no test results, no
/// job-URL lookup — so `contents: read` is the whole block beside the OIDC
/// scope.
const SIGNING_PATCH_PERMISSIONS: &str = "    permissions:\n      contents: read\n";

/// Whether this spec signs with an OIDC identity rather than a key.
///
/// Key mode signs with material the spec names and never exchanges a token,
/// so it takes no `id-token` scope — granting one would hand the job a
/// capability nothing in it uses.
fn signs_keyless(spec: &MirrorSpec) -> bool {
    spec.sign.as_ref().is_some_and(|sign| sign.keyless.is_some())
}

pub fn render_push_permissions(spec: &MirrorSpec) -> String {
    match (spec.target.registry == GHCR_REGISTRY, signs_keyless(spec)) {
        (true, false) => GHCR_PUSH_PERMISSIONS.to_string(),
        (true, true) => format!("{GHCR_PUSH_PERMISSIONS}{ID_TOKEN_PERMISSION}"),
        (false, false) => String::new(),
        (false, true) => format!("{SIGNING_PUSH_PERMISSIONS}{ID_TOKEN_PERMISSION}"),
    }
}

/// `permissions:` block for the patch job.
///
/// Splits off [`render_registry_write_permissions`], which describe and
/// cascade keep: patch re-emits published manifests and so signs them (C-071),
/// while describe publishes catalog metadata and cascade re-points tags —
/// neither pushes a package manifest, so neither takes the OIDC scope.
pub fn render_patch_permissions(spec: &MirrorSpec) -> String {
    match (spec.target.registry == GHCR_REGISTRY, signs_keyless(spec)) {
        (true, false) => GHCR_REGISTRY_WRITE_PERMISSIONS.to_string(),
        (true, true) => format!("{GHCR_REGISTRY_WRITE_PERMISSIONS}{ID_TOKEN_PERMISSION}"),
        (false, false) => String::new(),
        (false, true) => format!("{SIGNING_PATCH_PERMISSIONS}{ID_TOKEN_PERMISSION}"),
    }
}

/// The `env:` lines a signing step needs, appended to the block it already has.
///
/// `resolve_sign` reads every `env://NAME` the spec names out of the child
/// process's own environment, so the workflow is what has to put it there.
/// Secret-class refs — the key, its passphrase, an explicit identity token —
/// map from `secrets.`; the Fulcio and Rekor endpoints are URLs rather than
/// secrets and map from `vars.`, which keeps them readable in a run log.
/// A `file://` ref names a path on the runner and a literal is already the
/// value, so neither contributes a line.
///
/// Keyed by variable name rather than emitted in field order: two fields may
/// legitimately name one variable, and a repeated key would make GitHub reject
/// the workflow outright. Secrets are collected after vars so a name claimed by
/// both classes resolves to the more conservative of the two.
pub fn render_sign_env(spec: &MirrorSpec) -> String {
    let Some(sign) = spec.sign.as_ref() else {
        return String::new();
    };

    let mut vars: Vec<&Ref> = Vec::new();
    let mut secrets: Vec<&Ref> = Vec::new();
    if let Some(keyless) = &sign.keyless {
        vars.extend(keyless.fulcio.iter().chain(keyless.rekor.iter()));
        secrets.extend(keyless.identity_token.iter());
    }
    match &sign.key {
        Some(KeyConfig::Reference(reference)) => secrets.push(reference),
        Some(KeyConfig::Full(key)) => {
            secrets.push(&key.reference);
            secrets.extend(key.passphrase.iter());
            vars.extend(key.rekor.iter());
        }
        None => {}
    }

    let mut mapped: BTreeMap<&str, &str> = BTreeMap::new();
    for (references, context) in [(vars, "vars"), (secrets, "secrets")] {
        for reference in references {
            if let Ref::Env(name) = reference {
                mapped.insert(name, context);
            }
        }
    }
    if mapped.is_empty() {
        return String::new();
    }

    let mut rendered = String::from(
        "\n          # Signing material named by `sign:` in the spec — ocx-mirror resolves\n          # each `env://NAME` from this step's environment.",
    );
    for (name, context) in mapped {
        rendered.push_str(&format!("\n          {name}: ${{{{ {context}.{name} }}}}"));
    }
    rendered
}

/// `permissions:` block for the discover job.
///
/// Only `contents: read` (checkout, setup-ocx) and `packages: read` — discover
/// lists the target's tags and writes nothing.
pub const GHCR_DISCOVER_PERMISSIONS: &str = "    permissions:\n      contents: read\n      packages: read\n";

pub fn render_discover_permissions(spec: &MirrorSpec) -> &'static str {
    if spec.target.registry == GHCR_REGISTRY {
        GHCR_DISCOVER_PERMISSIONS
    } else {
        ""
    }
}

/// `permissions:` block for a job that checks out, installs ocx and writes to
/// the target registry: the describe job and the patch job.
///
/// `pipeline describe` pushes the catalog metadata as an `__ocx.desc` referrer
/// and `pipeline patch` re-emits published manifests, so GHCR needs
/// `packages: write` for both — the read scope discover gets is not enough.
/// Neither job runs tests, resolves a job URL or comments on a pull request, so
/// none of the push job's other three scopes are paid for here; naming any
/// permission sets every unnamed one to `none`, which is what makes that
/// omission real rather than decorative. The announce a patch chains into
/// writes to the *index* repository through `OCX_ANNOUNCE_TOKEN`, never through
/// this job's `GITHUB_TOKEN`.
pub const GHCR_REGISTRY_WRITE_PERMISSIONS: &str = "    permissions:\n      contents: read\n      packages: write\n";

pub fn render_registry_write_permissions(spec: &MirrorSpec) -> &'static str {
    if spec.target.registry == GHCR_REGISTRY {
        GHCR_REGISTRY_WRITE_PERMISSIONS
    } else {
        ""
    }
}

/// Registry-login step for the discover job.
///
/// `pipeline plan` reads the target's tag list to decide which versions are
/// new. GHCR answers an *unauthenticated* read of a repository that does not
/// exist — or is private — with `403 DENIED`, never `404`; it does not reveal
/// non-existence to anonymous callers. `list_target_tags` deliberately treats
/// only an authoritative not-found as "nothing published" (issue #157), so
/// without a credential here the very first run of a new GHCR mirror aborts in
/// discover and the target can never come into existence.
///
/// A public non-GHCR target lists anonymously, so no login is emitted there —
/// the shared `OCX_MIRROR_REGISTRY_*` secrets stay confined to the push job.
pub fn render_discover_auth_steps(spec: &MirrorSpec) -> String {
    if spec.target.registry != GHCR_REGISTRY {
        return String::new();
    }
    format!(
        r#"      # `pipeline plan` reads the target's tags. ghcr.io answers an
      # anonymous read of a missing or private repository with 403 DENIED
      # rather than 404, so an unauthenticated discover can never see the
      # empty target a first publish starts from. docker login so ocx picks
      # the credential up via its native-credential fallback.
      - name: Login to {ghcr}
        run: |
          echo "${{{{ secrets.GITHUB_TOKEN }}}}" \
            | docker login {ghcr} \
                -u "${{{{ github.actor }}}}" \
                --password-stdin
"#,
        ghcr = GHCR_REGISTRY,
    )
}

/// Best-effort warning that a `ghcr.io` target sits outside the publishing
/// repository's owner.
///
/// `GITHUB_TOKEN` authorises packages owned by *this repository's* owner only.
/// `docker login ghcr.io` succeeds either way — login does not authorise — so a
/// cross-owner target first surfaces as `denied: installation not allowed to
/// Create organization package` in the push job, and the GHCR credential probe
/// is a constant `have=true` with no honest skip branch to take.
///
/// `publishing_repo` is `GITHUB_REPOSITORY` (`owner/repo`). It is set on every
/// runner — the drift guard runs `generate ci --check` there — and absent when a
/// maintainer generates locally, where the owner is simply unknown and the
/// check yields nothing. Warn only: generate cannot always know the remote, and
/// a cross-owner push with a PAT is a legitimate (if unsupported) setup.
pub fn ghcr_owner_warning(spec: &MirrorSpec, publishing_repo: Option<&str>) -> Option<String> {
    if spec.target.registry != GHCR_REGISTRY {
        return None;
    }
    let publishing_owner = publishing_repo?.split('/').next()?.trim();
    let target_owner = spec.target.repository.split('/').next()?.trim();
    if publishing_owner.is_empty() || target_owner.is_empty() || publishing_owner.eq_ignore_ascii_case(target_owner) {
        return None;
    }
    Some(format!(
        "target {}/{} is owned by `{target_owner}` but this repository belongs to \
         `{publishing_owner}` — GITHUB_TOKEN only authorises packages under its own owner, \
         so the push will fail with `denied: installation not allowed to Create organization \
         package`. Publish under `{publishing_owner}`, or log in with a PAT that can write \
         `{target_owner}` packages.",
        GHCR_REGISTRY, spec.target.repository,
    ))
}

/// Credential-detection + registry-login steps, shared by the push job and the
/// describe job — both write to the target registry with the same credential.
///
/// GHCR is always credentialed: `GITHUB_TOKEN` is present on every run, so the
/// probe is a constant `have=true`. Without that, a GHCR push would take the
/// "no `OCX_MIRROR_REGISTRY_TOKEN`" branch and silently skip on every run.
/// Those org secrets hold `ocx.sh` credentials shared across every mirror
/// repository — repurposing them for GHCR would break all of them — so the
/// GHCR path never reads them.
pub fn render_registry_auth_steps(spec: &MirrorSpec) -> String {
    if spec.target.registry == GHCR_REGISTRY {
        return format!(
            r#"      # ghcr.io authenticates with this run's own GITHUB_TOKEN, which is
      # always present — so the credential probe is constant. The shared
      # OCX_MIRROR_REGISTRY_* org secrets hold {other} credentials and are
      # deliberately not read here.
      - name: Detect registry credentials
        id: creds
        run: echo "have=true" >> "${{GITHUB_OUTPUT}}"
      # docker login so ocx picks the credential up via its native-credential
      # fallback (`get_docker_auth` in crates/ocx_lib/src/auth.rs).
      - name: Login to {ghcr}
        run: |
          echo "${{{{ secrets.GITHUB_TOKEN }}}}" \
            | docker login {ghcr} \
                -u "${{{{ github.actor }}}}" \
                --password-stdin
"#,
            ghcr = GHCR_REGISTRY,
            other = "ocx.sh",
        );
    }

    format!(
        r#"      # Detect whether registry credentials are configured.
      # GitHub does not allow `secrets.*` in job-level `if:`, so we probe at
      # step level via env-var injection (secret value never echoed to logs).
      - name: Detect registry credentials
        id: creds
        env:
          OCX_MIRROR_REGISTRY_TOKEN: ${{{{ secrets.OCX_MIRROR_REGISTRY_TOKEN }}}}
        run: |
          if [ -n "${{OCX_MIRROR_REGISTRY_TOKEN}}" ]; then
            echo "have=true" >> "${{GITHUB_OUTPUT}}"
          else
            echo "have=false" >> "${{GITHUB_OUTPUT}}"
            echo "::notice::No OCX_MIRROR_REGISTRY_TOKEN secret — registry push skipped (repo runs in test/validation mode)."
          fi
      # Use docker login so ocx picks credentials up via its
      # native-credential fallback (`get_docker_auth` in crates/ocx_lib/src/auth.rs).
      # Env-var auth (`OCX_AUTH_<REG>_USER/_TOKEN`) takes precedence over the
      # docker fallback inside ocx, so do NOT also export those vars here.
      - name: Login to {registry}
        if: ${{{{ steps.creds.outputs.have == 'true' }}}}
        run: |
          echo "${{{{ secrets.OCX_MIRROR_REGISTRY_TOKEN }}}}" \
            | docker login {registry} \
                -u "${{{{ secrets.OCX_MIRROR_REGISTRY_USER }}}}" \
                --password-stdin
"#,
        registry = spec.target.registry,
    )
}
