// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! GHCR-specific job permissions and registry login steps.
//!
//! A mirror publishing to `ghcr.io` authenticates with the workflow's own
//! `GITHUB_TOKEN` and needs `packages:` scopes the default token does not
//! carry; every other registry uses explicit credentials and needs neither.

use crate::spec::MirrorSpec;

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

pub fn render_push_permissions(spec: &MirrorSpec) -> &'static str {
    if spec.target.registry == GHCR_REGISTRY {
        GHCR_PUSH_PERMISSIONS
    } else {
        ""
    }
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
