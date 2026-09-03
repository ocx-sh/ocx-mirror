// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline generate ci` — renders the GHA workflow and support
//! scripts from `mirror.yml` using baked-in templates.

mod aux_workflows;
mod drift;
mod matrix;
mod permissions;
mod slot;

// Glob rather than a named list: the `#[path]` test modules reach the
// renderer through `use super::super::*;`, and a glob keeps every child's
// surface visible to them without ci.rs having to name items it does not
// itself call (which `-D unused-imports` would then reject).
use aux_workflows::*;
use drift::*;
use matrix::*;
use permissions::*;
use slot::*;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ocx_lib::cli::DataInterface;

use crate::command::package::options::OutputFormat;
use crate::error::MirrorError;
use crate::spec::{self, MirrorSpec};

// ── Renderer (native + container legs) ───────────────────────────────────────
//
// A platform without `containers:` renders one native leg: tests run on the GHA
// runner against the ocx that setup-ocx put on PATH. A platform WITH
// `containers:` renders one leg per image, and every `ocx package test` for that
// leg runs inside `docker run <image>` with a libc-matched, statically-linked
// ocx release mounted in.
//
// The container wrapper is the whole point of the feature: `os.features` claims
// like musl vs glibc are unverifiable until the mirrored artifact is executed
// under that libc's loader. A leg that merely renders proves nothing.
//
// Native output is byte-identical to the pre-container renderer — the extra
// matrix keys and the docker prelude are emitted only when a leg carries an
// image, and `tests/golden/` asserts that for the whole native fixture corpus.

// ── Build-time constants ─────────────────────────────────────────────────────

/// OCX-mirror crate version baked in at compile time.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git SHA injected by `build.rs` via `OCX_GIT_SHA_SHORT`.
/// Falls back to `"unknown"` when the build environment has no git context.
const GIT_SHA_SHORT: &str = match option_env!("OCX_GIT_SHA_SHORT") {
    Some(sha) => sha,
    None => "unknown",
};

// ── Baked-in templates ───────────────────────────────────────────────────────

const WORKFLOW_TEMPLATE: &str = include_str!("templates/workflow.yml");

// ── Spec placement ───────────────────────────────────────────────────────────

// ── Public struct ────────────────────────────────────────────────────────────

/// Generate (or check) the CI workflow files for a mirror repository.
///
/// One repository may hold several specs; `--spec` repeats. Generated
/// filenames derive from where each spec sits under the repository root:
/// `<root>/mirror.yml` renders `mirror.yml`, `describe.yml`, `patch.yml`,
/// `cascade.yml` when it cascades and `announce-from-registry.yml` when it
/// announces, while
/// `<root>/py3.13/mirror.yml` renders the same set suffixed `-py3.13`. The
/// `verify-generated.yml` drift guard is
/// emitted once per repository and bakes in the full spec list, so the
/// committed file is the record of which specs the repository has.
///
/// In `--check` mode: exits 65 (DataError) if any generated file drifts from
/// what would be produced, or if a generated workflow belongs to no spec any
/// more; emits path-only hints to stderr.
#[derive(clap::Parser)]
pub struct GenerateCi {
    /// Path to the mirror spec file; repeat once per spec the repository holds.
    #[arg(long, default_value = DEFAULT_SPEC_PATH)]
    pub spec: Vec<PathBuf>,

    /// Repository root the workflows are written under [default: the directory every spec shares].
    #[arg(long)]
    pub repo_root: Option<PathBuf>,

    /// Check mode: verify generated files are up-to-date; exit 65 on drift.
    #[arg(long)]
    pub check: bool,

    /// Output format for diagnostics.
    #[arg(long)]
    pub format: Option<OutputFormat>,
}

impl GenerateCi {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        // Phases 1–3 per spec: raw pre-flight, structural load, content policy.
        let mut specs = Vec::with_capacity(self.spec.len());
        for path in &self.spec {
            specs.push((path.clone(), load_one(path).await?));
        }

        // Phase 4: place every spec — and every base it extends — under the
        // repository root, then render.
        let repo_root = self.resolve_repo_root().await?;
        let mut placed = Vec::with_capacity(specs.len());
        for (path, (spec, chain)) in specs {
            let mut bases = Vec::with_capacity(chain.len());
            for base in &chain {
                bases.push(canonical(base).await?);
            }
            let slot = SpecSlot::new(&canonical(&path).await?, &bases, &repo_root)?;
            placed.push((slot, spec));
        }
        // Sorted so neither the rendered bytes nor the drift verdict depend on
        // the order the specs were passed in.
        placed.sort_by(|(a, _), (b, _)| a.relative.cmp(&b.relative));
        reject_colliding_slots(&placed)?;

        let named = placed.len() > 1;
        // `script:` paths are the one thing only the repository root can resolve,
        // and this is the only command that knows it.
        let invalid: Vec<String> = placed
            .iter()
            .flat_map(|(slot, spec)| {
                spec::validate_test_scripts(spec, &repo_root, slot.dir())
                    .into_iter()
                    .map(|error| format!("{}{error}", label(slot, named)))
            })
            .collect();
        if !invalid.is_empty() {
            return Err(MirrorError::SpecInvalid(invalid));
        }

        for (slot, spec) in &placed {
            if let Some(warning) = ghcr_owner_warning(spec, std::env::var("GITHUB_REPOSITORY").ok().as_deref()) {
                eprintln!("warning: {}{warning}", label(slot, named));
            }
        }
        report_manual_edits(&placed);

        let files = render(&placed);

        // Phase 5: write or check.
        if self.check {
            check_drift(&files, &repo_root).await
        } else {
            write_files(&files, &repo_root).await
        }
    }

    /// The directory the generated workflows are written under.
    ///
    /// An explicit `--repo-root` wins. Otherwise it is the enclosing git
    /// repository, which is the same answer for one nested spec as for five.
    /// Inferring it from the spec set is not: the deepest directory a *single*
    /// nested spec shares is its own parent, so a repo-root-relative
    /// `tests: script:` had the spec directory prepended twice
    /// (`repo/tool/tool/tests/smoke.star`) and an `extends:` base above the
    /// spec read as outside the repository. Every single-spec-in-a-subdirectory
    /// repo failed its own drift guard; five-spec repos passed only because
    /// their common ancestor happened to be the real root.
    ///
    /// Falls back to the shared ancestor outside a git repository — rendering
    /// into a bare directory has no better answer, and the process CWD would be
    /// worse: `generate ci --spec /elsewhere/repo/mirror.yml` must write into
    /// that repository, not wherever it was invoked from.
    async fn resolve_repo_root(&self) -> Result<PathBuf, MirrorError> {
        if let Some(root) = &self.repo_root {
            return canonical(root).await;
        }
        let mut shared: Option<PathBuf> = None;
        for path in &self.spec {
            let parent = canonical(spec_parent(path)).await?;
            if let Some(root) = git_root(&parent) {
                // A spec outside this root is caught by `SpecSlot::new`, which
                // is a better error than silently rooting at an ancestor of two
                // unrelated repositories.
                return Ok(root);
            }
            shared = Some(match shared {
                None => parent,
                Some(current) => common_ancestor(&current, &parent),
            });
        }
        shared.ok_or_else(|| MirrorError::SpecUsageError("no --spec given".to_string()))
    }
}

/// Read, pre-flight and validate one spec file (phases 1–3).
///
/// Returns the merged spec alongside its `extends:` chain — the merged spec has
/// no record of where its keys came from, and the renderer needs the base paths
/// to trigger on them.
async fn load_one(path: &Path) -> Result<(MirrorSpec, Vec<PathBuf>), MirrorError> {
    // Phase 1: policy-level pre-flight before load_spec.
    //
    // Check for `ocx_install:` key in the raw YAML text. MirrorSpec uses
    // `#[serde(deny_unknown_fields)]` so load_spec would emit SpecInvalid (65),
    // but plan §1.8 requires SpecUsageError (64) for this specific case.
    // Peeking the raw bytes lets us intercept before serde rejects it.
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", path.display())))?;

    if raw.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("ocx_install:") || trimmed == "ocx_install:"
    }) {
        return Err(MirrorError::SpecUsageError(
            "ocx binary is installed via direct release download; \
             remove `ocx_install:` block. \
             Override `OCX_BINARY_OVERRIDE` env var at workflow level for integration tests"
                .to_string(),
        ));
    }

    // Phase 2: load and validate spec (structural validation).
    let chain = spec::resolve_extends_chain(path, &raw).await?;
    let spec = spec::load_spec(path).await?;

    // Phase 3: content-policy validation on the parsed spec.
    if let Some(notify) = &spec.notify {
        spec::policy_check_notify(notify)?;
    }

    Ok((spec, chain))
}

/// Reject two specs that would render the same workflow set.
///
/// Output names come from the spec's *directory*, so two specs sharing one
/// directory would silently overwrite each other — the exact failure repeatable
/// `--spec` exists to fix. Expects `placed` sorted by relative path.
fn reject_colliding_slots(placed: &[(SpecSlot, MirrorSpec)]) -> Result<(), MirrorError> {
    for pair in placed.windows(2) {
        let (first, second) = (&pair[0].0, &pair[1].0);
        if first.suffix() == second.suffix() {
            return Err(MirrorError::SpecUsageError(format!(
                "specs `{}` and `{}` would render the same workflow files — \
                 generated names derive from the spec's directory, so each spec needs its own",
                first.source(),
                second.source(),
            )));
        }
    }
    Ok(())
}

/// Surface the discouraged `allow_manual_edits` opt-out so it is never silently
/// in effect: the drift guard is the only thing keeping generated workflows
/// honest.
///
/// One guard covers the whole repository, so a repository whose specs disagree
/// gets the guard — the strictest spec wins, and the opt-out only takes effect
/// when every spec asks for it. Naming the dissenters is what makes an
/// unexpectedly-present guard explicable.
fn report_manual_edits(placed: &[(SpecSlot, MirrorSpec)]) {
    let opted_out: Vec<String> = placed
        .iter()
        .filter(|(_, spec)| spec.allow_manual_edits)
        .map(|(slot, _)| slot.source())
        .collect();
    if opted_out.is_empty() {
        return;
    }
    if opted_out.len() == placed.len() {
        eprintln!(
            "note: allow_manual_edits is set — the generated-workflow drift guard \
             (verify-generated.yml) is not emitted; hand-edits to generated workflows \
             go unchecked (discouraged)"
        );
    } else {
        eprintln!(
            "warning: allow_manual_edits is set on {} but not on every spec — one drift guard \
             covers the whole repository, so verify-generated.yml is emitted anyway and \
             hand-edits to those specs' workflows still fail CI",
            opted_out.join(", "),
        );
    }
}

// ── Policy validation ────────────────────────────────────────────────────────

// ── Renderer ─────────────────────────────────────────────────────────────────

/// Render the GHA workflow YAML from a parsed spec.
///
/// Substitution uses a simple `str::replace` chain — no templating engine dep.
fn render_workflow(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    let schedule_block = schedule_block(spec.versions.as_ref().and_then(|v| v.poll_interval.as_ref()));

    // `webhook_secret` names the *GitHub Actions secret* that carries the
    // webhook URL — the rendered workflow maps it onto the conventional local
    // env var `OCX_MIRROR_DISCORD_HOOK`, which `pipeline notify` reads.
    let webhook_secret_name = spec
        .notify
        .as_ref()
        .and_then(|n| n.discord.as_ref())
        .map(|d| d.webhook_secret.as_str())
        .unwrap_or("OCX_MIRROR_DISCORD_HOOK");

    // The Discord user id is non-secret — inline it verbatim into the notify
    // job env. Absent → the placeholder collapses to nothing so the env block
    // carries only the webhook hook line.
    let discord_user_id_env = spec
        .notify
        .as_ref()
        .and_then(|n| n.discord.as_ref())
        .and_then(|d| d.user_id.as_ref())
        .map(|id| format!("\n          OCX_MIRROR_DISCORD_USER_ID: \"{id}\""))
        .unwrap_or_default();

    // Env sources (`pylock`, `pypi`) publish an env package — composed metadata
    // plus N wheel layers — where every other source publishes one archive
    // bundle per platform. Three points in the workflow differ because of it:
    // what `prepare` gathers, what `test` hands `ocx package test`, and (pypi
    // only, whose lock is derived in-pipeline rather than committed) what the
    // discover job's plan artifact carries. Everything else is source-agnostic,
    // and an archive spec renders exactly the bytes it rendered before.
    let is_env = spec.source.is_env();
    let is_pypi = matches!(spec.source, spec::Source::Pypi { .. });

    let matrix = build_matrix(spec);
    let matrix_entries = render_matrix_entries(&matrix);
    let test_run_steps = render_test_run_steps(&matrix, is_env);
    let target_identifier = spec.target.reference();

    // The Dockerfile reaches the shell through `env:`, not an inline `${{ }}`:
    // it is multi-line and carries the setup commands' own quoting, neither of
    // which survives interpolation into a shell script. Absent → the
    // placeholder collapses and the env block is the one line it always was.
    let container_setup_env = if any_container_setup(&matrix) {
        "\n          OCX_CONTAINER_DOCKERFILE: ${{ matrix.container_dockerfile }}".to_string()
    } else {
        String::new()
    };

    let triggers = trigger_paths(
        slot,
        &[
            slot.source(),
            "scripts/**".to_string(),
            "tests/**".to_string(),
            "metadata*.json".to_string(),
        ],
        "mirror",
    );

    WORKFLOW_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{TRIGGER_PATHS}", &triggers)
        .replace("{MIRROR_NAME}", &spec.name)
        .replace("{SCHEDULE_BLOCK}", &schedule_block)
        .replace("{PLAN_ARTIFACT_PATH}", plan_artifact_path(is_pypi))
        .replace("{DERIVED_LOCKS_ARTIFACT}", &derived_locks_artifact(is_pypi))
        .replace("{PREPARE_FLATTEN}", prepare_flatten_script(is_env))
        .replace("{TEST_MATRIX_ENTRIES}", &matrix_entries)
        .replace("{TEST_TARGET_RESOLVE}", test_target_resolve_script(is_env))
        .replace("{TEST_RUN_STEPS}", &test_run_steps)
        // Substituted after `{TEST_RUN_STEPS}` — the placeholder lives inside the
        // container prelude that step just injected.
        .replace("{OCX_CLI_TAG}", OCX_CONTAINER_CLI_TAG)
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
        .replace("{TARGET_IDENTIFIER}", &target_identifier)
        .replace("{TARGET_REGISTRY}", &spec.target.registry)
        .replace("{DISCOVER_PERMISSIONS}", render_discover_permissions(spec))
        .replace("{DISCOVER_AUTH_STEPS}", &render_discover_auth_steps(spec))
        .replace("{PUSH_PERMISSIONS}", &render_push_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{SIGN_ENV}", &render_sign_env(spec))
        .replace("{WEBHOOK_SECRET_NAME}", webhook_secret_name)
        .replace("{DISCORD_USER_ID_ENV}", &discord_user_id_env)
        .replace("{CONTAINER_SETUP_ENV}", &container_setup_env)
}

/// Build the map of repo-root-relative path → file content for one spec.
fn render_spec(spec: &MirrorSpec, slot: &SpecSlot) -> BTreeMap<PathBuf, String> {
    let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();

    files.insert(slot.workflow("mirror"), render_workflow(spec, slot));
    files.insert(slot.workflow("describe"), render_describe(spec, slot));
    // Emitted for every spec, with no opt-in: any published mirror can have its
    // metadata drift, and a repository only discovers it needs the workflow at
    // the moment it already needs to have dispatched it.
    files.insert(slot.workflow("patch"), render_patch(spec, slot));

    // Rolling-tag repair: only a spec that cascades has aliases to break.
    if spec.cascade.enabled {
        files.insert(slot.workflow("cascade"), render_cascade(spec, slot));
    }

    // Index catch-up workflow: only a mirror that announces has an index entry
    // to catch up. Emitted for every such mirror — there is no separate opt-in,
    // because a mirror that opted into `announce:` after publishing is exactly
    // the one that needs it, and it cannot know that about itself.
    if spec.announce.is_some() {
        files.insert(
            slot.workflow("announce-from-registry"),
            render_announce_from_registry(spec, slot),
        );
    }

    files
}

/// Build the full map of relative path → file content for every generated file.
///
/// Keys are relative to the repository root. Every spec contributes its own
/// workflow set; the repository contributes one drift guard, skipped only when
/// *all* specs opt out via `allow_manual_edits` (discouraged) — a single guard
/// covers every workflow, so one spec still wanting it is enough to emit it.
fn render(placed: &[(SpecSlot, MirrorSpec)]) -> BTreeMap<PathBuf, String> {
    let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
    for (slot, spec) in placed {
        files.extend(render_spec(spec, slot));
    }

    if placed.iter().any(|(_, spec)| !spec.allow_manual_edits) {
        let slots: Vec<&SpecSlot> = placed.iter().map(|(slot, _)| slot).collect();
        files.insert(
            PathBuf::from(".github/workflows/verify-generated.yml"),
            render_verify_generated(&slots),
        );
    }

    files
}

// ── Writer ────────────────────────────────────────────────────────────────────

// ── Drift detector ────────────────────────────────────────────────────────────

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "ci/tests.rs"]
mod tests;
