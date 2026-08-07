// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline generate ci` — renders the GHA workflow and support
//! scripts from `mirror.yml` using baked-in templates.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ocx_lib::cli::DataInterface;

use crate::command::package::options::OutputFormat;
use crate::error::MirrorError;
use crate::spec::{self, MirrorSpec, PlatformConfig, TestEntry};

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
const DESCRIBE_TEMPLATE: &str = include_str!("templates/describe.yml");
const VERIFY_GENERATED_TEMPLATE: &str = include_str!("templates/verify-generated.yml");
const ANNOUNCE_FROM_REGISTRY_TEMPLATE: &str = include_str!("templates/announce-from-registry.yml");
const PATCH_TEMPLATE: &str = include_str!("templates/patch.yml");
const CASCADE_TEMPLATE: &str = include_str!("templates/cascade.yml");

// ── Spec placement ───────────────────────────────────────────────────────────

/// What `--spec` defaults to, and therefore the one spec path a generated
/// invocation can leave unsaid.
const DEFAULT_SPEC_PATH: &str = "./mirror.yml";

/// The repo-root-relative spec path that `DEFAULT_SPEC_PATH` names.
const DEFAULT_SPEC_NAME: &str = "mirror.yml";

/// Where one spec's generated workflows land, and how they refer back to it.
///
/// Everything is derived from the spec's path *relative to the repository
/// root* — never from its position in the argument list, so passing the same
/// specs in a different order renders the same bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpecSlot {
    /// Spec path relative to the repo root: `mirror.yml`, `py3.13/mirror.yml`.
    relative: PathBuf,
    /// This spec's whole `extends:` chain, repo-root-relative, nearest base
    /// first. Editing any of them changes what this spec renders and publishes,
    /// so all of them belong in its `paths:` trigger.
    extends: Vec<PathBuf>,
}

impl SpecSlot {
    fn new(spec: &Path, extends: &[PathBuf], repo_root: &Path) -> Result<Self, MirrorError> {
        let relative = spec.strip_prefix(repo_root).map_err(|_| {
            MirrorError::SpecUsageError(format!(
                "spec {} is not under the repository root {} — pass --repo-root",
                spec.display(),
                repo_root.display(),
            ))
        })?;
        // A base above the root is the same failure as a spec above the root,
        // one step further out: `paths:` can only name files the workflow's own
        // repository contains, so a trigger for it would silently never fire —
        // which is exactly what putting the chain in the trigger is fixing.
        let extends = extends
            .iter()
            .map(|base| {
                base.strip_prefix(repo_root).map(Path::to_path_buf).map_err(|_| {
                    MirrorError::SpecUsageError(format!(
                        "spec {} extends {}, which is not under the repository root {} — \
                         a `paths:` trigger cannot name a file outside the repository, so editing \
                         that base would run nothing; move it under the root or pass --repo-root",
                        spec.display(),
                        base.display(),
                        repo_root.display(),
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            relative: relative.to_path_buf(),
            extends,
        })
    }

    /// The `extends` chain as `paths:` entries, dropping any base the spec's own
    /// subtree glob already covers.
    fn extends_entries(&self) -> Vec<String> {
        let own_subtree = self.dir().map(|dir| format!("{}/", slash_path(dir)));
        self.extends
            .iter()
            .map(|base| slash_path(base))
            .filter(|base| !own_subtree.as_ref().is_some_and(|prefix| base.starts_with(prefix)))
            .collect()
    }

    /// The spec's own directory relative to the repo root; `None` at the root.
    fn dir(&self) -> Option<&Path> {
        self.relative.parent().filter(|dir| !dir.as_os_str().is_empty())
    }

    /// Filename suffix for this spec's workflows — empty at the repo root,
    /// `-py3.13` for `py3.13/mirror.yml`, `-a-b` for `a/b/mirror.yml`.
    fn suffix(&self) -> String {
        match self.dir() {
            None => String::new(),
            Some(dir) => format!("-{}", slash_path(dir).replace('/', "-")),
        }
    }

    /// Repo-root-relative path of one of this spec's generated workflows.
    fn workflow(&self, stem: &str) -> PathBuf {
        PathBuf::from(format!(".github/workflows/{}", self.workflow_name(stem)))
    }

    /// Bare filename of one of this spec's generated workflows.
    fn workflow_name(&self, stem: &str) -> String {
        format!("{stem}{}.yml", self.suffix())
    }

    /// The spec path as the generated workflows spell it: repo-root-relative
    /// and `/`-separated, because they are read on a Linux runner.
    fn source(&self) -> String {
        slash_path(&self.relative)
    }

    /// `--spec <path>` threaded into this spec's generated invocations.
    ///
    /// Empty for the repo-root `mirror.yml`, whose path is exactly what every
    /// pipeline command already defaults to — omitting it is what keeps the
    /// published mirror repositories byte-identical to what they have
    /// committed. Any other location is named explicitly.
    fn spec_arg(&self) -> String {
        if self.is_default() {
            String::new()
        } else {
            format!(" --spec {}", self.source())
        }
    }

    fn is_default(&self) -> bool {
        self.relative == Path::new(DEFAULT_SPEC_NAME)
    }
}

/// `Path` → `/`-separated string. Generated workflows run on a Linux runner,
/// so the separator is never the generating host's.
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

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

/// The nearest ancestor of `dir` holding a `.git`, or `None` outside a
/// repository.
///
/// `.git` is a directory in a normal clone and a *file* in a worktree or
/// submodule checkout, so existence is the test, not file type.
///
/// ponytail: sync `exists()` in an async fn — one short upward walk, once per
/// invocation. Reach for `tokio::fs` here only if this ever runs per-spec.
fn git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
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
    policy_check_notify(&spec)?;

    Ok((spec, chain))
}

/// Resolve a path against the filesystem so spec paths and the repository root
/// are comparable regardless of which of them the caller spelled relatively.
async fn canonical(path: &Path) -> Result<PathBuf, MirrorError> {
    tokio::fs::canonicalize(path)
        .await
        .map_err(|e| MirrorError::SpecUsageError(format!("cannot resolve {}: {e}", path.display())))
}

/// The directory holding `spec`, with a bare filename resolving to `.`.
fn spec_parent(spec: &Path) -> &Path {
    match spec.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Deepest directory both paths lie under. Both are canonical, so they share at
/// least the filesystem root.
fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    a.components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
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

/// `"<spec>: "` when the run covers several specs, so a warning names the one
/// it is about; empty for the single-spec repository, where it would be noise.
fn label(slot: &SpecSlot, named: bool) -> String {
    if named {
        format!("{}: ", slot.source())
    } else {
        String::new()
    }
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

/// Content-policy check on the `notify:` block.
///
/// Delegates to `spec::policy_check_notify` so the check logic lives in one place
/// and always returns `SpecUsageError (64)` for URL-literal webhook secrets.
/// `load_spec` already calls this before structural validation, so this call in
/// the renderer is a defence-in-depth guard for specs loaded via other paths.
fn policy_check_notify(spec: &MirrorSpec) -> Result<(), MirrorError> {
    let Some(notify) = &spec.notify else {
        return Ok(());
    };
    spec::policy_check_notify(notify)
}

// ── Renderer ─────────────────────────────────────────────────────────────────

/// The kind of a rendered test entry — mirrors [`spec::TestKind`] but owns its
/// payload so it can outlive the spec borrow in `MatrixLeg`.
#[derive(Debug, Clone, PartialEq)]
enum RenderedTestKind {
    Command(String),
    Script(String),
    ScriptInline(String),
}

/// One rendered test entry carried in a matrix leg.
#[derive(Debug, Clone)]
struct RenderedTest {
    name: String,
    kind: RenderedTestKind,
}

/// Describes one matrix leg (test job matrix entry).
///
/// A native leg has an empty `container_image` and the sentinel `container_id`
/// `_native_`. A container leg carries the image, its libc family (which ocx
/// release binary to mount) and a stable `container_id` taken from the config
/// `id` or the slugified image. Downstream consumers (`pipeline push`,
/// `junit.rs`) key on `(version, platform, container)` triples in JUnit XML and
/// run-summary.json, so `container_id` stays meaningful in both modes.
struct MatrixLeg {
    platform: String,
    platform_slug: String,
    runner: String,
    container_id: String,
    /// Container image reference; empty for a native leg.
    container_image: String,
    /// Container libc family (`musl` / `gnu`); empty for a native leg.
    container_libc: String,
    /// `platform` with any `os.features` suffix stripped — what
    /// `docker run --platform` accepts. Empty for a native leg.
    docker_platform: String,
    /// Dockerfile provisioning this leg's image from the container's `setup:`
    /// commands. Empty when the container declares none — which is also what
    /// gates every piece of setup machinery out of the rendered workflow.
    container_dockerfile: String,
    shell: String,
    tests: Vec<RenderedTest>,
}

/// Render the Dockerfile that provisions one container leg.
///
/// `RUN` in shell form hands the line to the image's `SHELL` unparsed, so the
/// YAML → Rust → Dockerfile → shell trip copies the command through. The shapes
/// that would not arrive as one `RUN` — an embedded newline, a trailing
/// backslash — are rejected by `validate_container_setup` before the renderer
/// sees them. A `${{ … }}` in a command is emitted raw and interpolated by
/// Actions, the same surface `tests[].command` already has.
fn render_setup_dockerfile(image: &str, shell: &str, setup: &[String]) -> String {
    // `{shell:?}` is the JSON-quoted exec form `SHELL` requires.
    let mut dockerfile = format!("FROM {image}\nSHELL [{shell:?}, \"-c\"]\n");
    for command in setup {
        dockerfile.push_str(&format!("RUN {command}\n"));
    }
    dockerfile
}

/// Does any leg provision its image?
///
/// One gate for the whole feature: the matrix key, the `env:` passthrough and
/// the build block are meaningless apart, so they appear together or not at
/// all — and a spec declaring no `setup:` renders exactly what it did before.
fn any_container_setup(legs: &[MatrixLeg]) -> bool {
    legs.iter().any(|leg| !leg.container_dockerfile.is_empty())
}

/// Convert a slice of [`TestEntry`] into [`RenderedTest`] list.
///
/// Entries that fail `kind()` (i.e. validated-invalid specs that slip through)
/// are silently omitted — `validate_tests` is the authoritative gate.
fn render_tests(entries: &[TestEntry]) -> Vec<RenderedTest> {
    entries
        .iter()
        .filter_map(|t| {
            let kind = match t.kind() {
                Ok(spec::TestKind::Command(cmd)) => RenderedTestKind::Command(cmd.to_owned()),
                Ok(spec::TestKind::Script(p)) => RenderedTestKind::Script(p.display().to_string()),
                Ok(spec::TestKind::ScriptInline(src)) => RenderedTestKind::ScriptInline(src.to_owned()),
                Err(_) => return None,
            };
            Some(RenderedTest {
                name: t.name.clone(),
                kind,
            })
        })
        .collect()
}

/// Build the flat list of matrix legs from a `MirrorSpec`.
fn build_matrix(spec: &MirrorSpec) -> Vec<MatrixLeg> {
    let Some(platforms) = &spec.platforms else {
        return Vec::new();
    };

    let top_level_tests: Vec<RenderedTest> = render_tests(spec.tests.as_deref().unwrap_or(&[]));

    // Stable ordering: sort platform keys alphabetically so the generated YAML
    // is deterministic across runs.
    let mut platform_keys: Vec<&String> = platforms.keys().collect();
    platform_keys.sort();

    let mut legs = Vec::new();
    for platform_key in platform_keys {
        let config = &platforms[platform_key];
        // Must equal the basename `pipeline prepare` gives this platform's work
        // directory — the workflow flattens that into `bundle-{V}-{slug}.tar.xz`
        // and names the leg's JUnit file with it. A libc-bearing key slugs to
        // `linux_amd64_libc.musl` there, so `replace('/', "_")` here pointed the
        // leg at a bundle that does not exist.
        let platform_slug = spec::platform_key_slug(platform_key);

        let effective_tests: Vec<RenderedTest> = config
            .tests
            .as_deref()
            .map(render_tests)
            .unwrap_or_else(|| top_level_tests.clone());

        match config.containers.as_deref().filter(|c| !c.is_empty()) {
            // Container mode: one leg per image, each testing the same artifact
            // under a different userland.
            Some(containers) => {
                for container in containers {
                    // Same slug `pipeline push` uses to find this leg's JUnit
                    // file. Diverging here loses every container result.
                    let container_id = container
                        .id
                        .clone()
                        .unwrap_or_else(|| spec::image_to_container_id(&container.image));
                    // Validation guarantees an explicit shell whenever the image
                    // has no known default, so the fallback is unreachable for a
                    // validated spec; POSIX `sh` is the safest thing to guess.
                    let shell = container.shell.clone().unwrap_or_else(|| {
                        spec::infer_shell_from_image(&container.image)
                            .unwrap_or("sh")
                            .to_string()
                    });
                    // Validation rejects an empty `setup:`, so the filter only
                    // guards against a spec that skipped it — an empty list
                    // must render as "no setup", never as a bare `FROM`.
                    let container_dockerfile = container
                        .setup
                        .as_deref()
                        .filter(|setup| !setup.is_empty())
                        .map(|setup| render_setup_dockerfile(&container.image, &shell, setup))
                        .unwrap_or_default();
                    legs.push(MatrixLeg {
                        platform: platform_key.clone(),
                        platform_slug: platform_slug.clone(),
                        runner: config.runner.clone(),
                        container_id,
                        container_image: container.image.clone(),
                        container_libc: spec::infer_libc_from_image(&container.image).to_string(),
                        docker_platform: spec::platform_without_features(platform_key),
                        container_dockerfile,
                        shell,
                        tests: effective_tests.clone(),
                    });
                }
            }
            None => {
                let shell = native_shell_for_platform(platform_key, config);
                legs.push(MatrixLeg {
                    platform: platform_key.clone(),
                    platform_slug: platform_slug.clone(),
                    runner: config.runner.clone(),
                    container_id: "_native_".to_string(),
                    container_image: String::new(),
                    container_libc: String::new(),
                    docker_platform: String::new(),
                    container_dockerfile: String::new(),
                    shell: shell.to_string(),
                    tests: effective_tests,
                });
            }
        }
    }
    legs
}

/// Determine the shell for a native test leg.
fn native_shell_for_platform<'a>(platform: &str, config: &'a PlatformConfig) -> &'a str {
    if let Some(shell) = &config.shell {
        return shell.as_str();
    }
    if platform.starts_with("windows") {
        "pwsh"
    } else {
        "bash"
    }
}

/// Render `on.*.paths` entries for one spec.
///
/// A repo-root spec keeps the repo-wide list it has always had. A spec in a
/// subdirectory watches that subtree instead, so editing `py3.13/` never wakes
/// the sibling specs' workflows. `root_entries` is the root spec's list minus
/// its own workflow file, which is appended for both cases.
///
/// Every base in the spec's `extends:` chain is watched too: the shared base of
/// a multi-spec repository sits *outside* every child's subtree, so a change to
/// the platform matrix or the container set there would otherwise re-run
/// nothing at all.
fn trigger_paths(slot: &SpecSlot, root_entries: &[String], stem: &str) -> String {
    let mut entries = match slot.dir() {
        None => root_entries.to_vec(),
        Some(dir) => vec![format!("{}/**", slash_path(dir))],
    };
    entries.extend(slot.extends_entries());
    entries.push(format!(".github/workflows/{}", slot.workflow_name(stem)));

    // A nested spec's subtree trigger only covers files under that subtree,
    // while a test's `script:` path resolves from the repository root — unlike
    // `metadata.default` and `catalog.*`, which resolve against the spec's own
    // directory. Say so where the gap is, or the first Starlark test in a
    // subdirectory spec silently stops triggering its own workflow.
    let note = match slot.dir() {
        None => String::new(),
        Some(dir) => format!(
            "      # `script:` paths resolve from the repository root, not from {0}/ —\n\
             \x20     # keep this spec's scripts under {0}/ so editing one triggers this run.\n",
            slash_path(dir),
        ),
    };

    format!("{note}{}", indent_entries(&entries))
}

/// Format path entries as a YAML sequence under `paths:` (no trailing newline —
/// the templates supply the surrounding lines).
fn indent_entries(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| format!("      - {entry}"))
        .collect::<Vec<_>>()
        .join("\n")
}

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
        .replace("{PUSH_PERMISSIONS}", render_push_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{WEBHOOK_SECRET_NAME}", webhook_secret_name)
        .replace("{DISCORD_USER_ID_ENV}", &discord_user_id_env)
        .replace("{CONTAINER_SETUP_ENV}", &container_setup_env)
}

/// GitHub's own container registry — authenticated with the workflow's
/// `GITHUB_TOKEN`, not with the shared `OCX_MIRROR_REGISTRY_*` org secrets.
const GHCR_REGISTRY: &str = "ghcr.io";

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
const GHCR_PUSH_PERMISSIONS: &str = "    permissions:\n      contents: read\n      packages: write\n      actions: read\n      checks: write\n      pull-requests: write\n";

fn render_push_permissions(spec: &MirrorSpec) -> &'static str {
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
const GHCR_DISCOVER_PERMISSIONS: &str = "    permissions:\n      contents: read\n      packages: read\n";

fn render_discover_permissions(spec: &MirrorSpec) -> &'static str {
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
const GHCR_REGISTRY_WRITE_PERMISSIONS: &str = "    permissions:\n      contents: read\n      packages: write\n";

fn render_registry_write_permissions(spec: &MirrorSpec) -> &'static str {
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
fn render_discover_auth_steps(spec: &MirrorSpec) -> String {
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
fn ghcr_owner_warning(spec: &MirrorSpec, publishing_repo: Option<&str>) -> Option<String> {
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
fn render_registry_auth_steps(spec: &MirrorSpec) -> String {
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

/// Render the YAML matrix `include:` entries for the test job.
///
/// Test commands are inlined as a YAML list so the workflow references them
/// via `${{ matrix.tests }}`. This ensures per-platform test overrides
/// (e.g. `cmake.exe --version` on `windows/amd64`) appear verbatim in the
/// generated YAML, satisfying golden-test assertions.
fn render_matrix_entries(legs: &[MatrixLeg]) -> String {
    let mut out = String::new();
    for leg in legs {
        out.push_str(&format!(
            "          - platform: {}\n            platform_slug: {}\n            runner: {}\n            container_id: {}\n",
            leg.platform, leg.platform_slug, leg.runner, leg.container_id,
        ));
        // Emitted only for container legs, so a native-only workflow keeps the
        // exact key set it had before container mode existed (zero drift for the
        // pinned mirror corpus). The test step reads an absent/empty
        // `container_image` as native mode.
        if !leg.container_image.is_empty() {
            out.push_str(&format!(
                "            container_image: {:?}\n            container_libc: {:?}\n            docker_platform: {:?}\n",
                leg.container_image, leg.container_libc, leg.docker_platform,
            ));
        }
        // Second guarded block, for the same reason: a container leg that
        // declares no `setup:` keeps the exact key set it had before the field
        // existed, so provisioning is opt-in per container, not per leg set.
        if !leg.container_dockerfile.is_empty() {
            // Block scalar `|` — the Dockerfile is multi-line and carries
            // whatever quoting the setup commands do. Body at 14 spaces
            // (matrix entry indent 12 + 2), same shape as `script_inline`.
            let indented = leg
                .container_dockerfile
                .lines()
                .map(|line| format!("              {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&format!("            container_dockerfile: |\n{indented}\n"));
        }
        out.push_str(&format!("            shell: {}\n", leg.shell));
        // Inline the test entries so they are visible in the generated YAML.
        out.push_str("            tests:\n");
        for test in &leg.tests {
            match &test.kind {
                RenderedTestKind::Command(cmd) => {
                    out.push_str(&format!(
                        "              - name: {}\n                kind: command\n                command: {}\n",
                        test.name, cmd
                    ));
                }
                RenderedTestKind::Script(path) => {
                    out.push_str(&format!(
                        "              - name: {}\n                kind: script\n                script: {}\n",
                        test.name, path
                    ));
                }
                RenderedTestKind::ScriptInline(src) => {
                    // Use YAML block scalar `|` so multi-line Starlark survives.
                    // Each line of the inline source is indented 18 spaces
                    // (matrix entry indent 14 + 4 for block scalar body).
                    let indented = src
                        .lines()
                        .map(|line| format!("                  {line}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push_str(&format!(
                        "              - name: {}\n                kind: script_inline\n                script_inline: |\n{indented}\n",
                        test.name
                    ));
                }
            }
        }
    }
    out
}

/// The one `ocx` CLI release a generated workflow runs, on every leg.
///
/// Both legs read it: native legs pass it to `setup-ocx` as its `version:`
/// input, container legs download the statically-linked release of the same tag
/// and mount it into the image. Left unpinned, the native legs would float to
/// whatever `ocx` is newest on the day a mirror happens to run while the
/// container legs stayed on this constant — the two halves of one test matrix
/// exercising different binaries.
///
/// Deliberately a renderer constant and not a spec field. Which `ocx` executes
/// the tests is a property of *this renderer* — it has to be able to read the
/// bundles this version writes — so it moves when the renderer moves, via each
/// repository's `ocx.lock`. A per-repo knob would let forty mirrors drift onto
/// forty different test binaries, which is the failure this pin exists to
/// prevent.
///
/// The trailing marker is the Renovate anchor; see `customManagers` in
/// `renovate.json`. Keep the literal on one line or the regex stops matching.
const OCX_CONTAINER_CLI_TAG: &str = "v0.5.6"; // renovate: datasource=github-releases depName=ocx-sh/ocx

/// [`OCX_CONTAINER_CLI_TAG`] as `setup-ocx` spells it: a bare semver, no `v`.
///
/// Derived rather than a second constant so the two spellings cannot drift —
/// Renovate only ever moves the tag.
fn ocx_cli_version() -> &'static str {
    OCX_CONTAINER_CLI_TAG.trim_start_matches('v')
}

/// Render per-test shell commands for the `test` job's run step.
///
/// Each matrix leg runs all its tests for every discovered version. The renderer
/// emits a single shell block that iterates per-version.
///
/// A native leg (empty `container_image`) calls `ocx package test` directly on
/// the runner. A container leg fetches a libc-matched, statically-linked `ocx`
/// release and wraps every `ocx package test` in `docker run <image>` — so the
/// mirrored artifact is actually executed by that image's loader against that
/// image's libc, which is the only way an `os.features` musl/glibc claim can be
/// verified. JS actions keep running on the host's glibc node throughout, which
/// is why this is a per-command wrapper rather than a job-level `container:`.
///
/// `is_env` switches what is handed to `ocx package test`: one bundle path for
/// an archive/binary source, `-m <metadata> <layers…>` for an env source, whose
/// artifact is a composed metadata document plus N wheel layers. Both forms are
/// resolved into shell variables by [`test_target_resolve_script`].
fn render_test_run_steps(legs: &[MatrixLeg], is_env: bool) -> String {
    if legs.is_empty() {
        return String::new();
    }

    // What `ocx package test` is pointed at, and the `--platform` it declares.
    // An env container leg names its own libc as an os_feature: `ocx package
    // test` threads `--platform` verbatim into DEPENDENCY resolution, and an
    // env's interpreter index may carry per-libc entries only — a bare
    // `linux/amd64` request would match none of them. Archive legs keep the
    // bare matrix platform, so their output is unchanged.
    let (test_target, test_platform) = if is_env {
        (r#"-m "${METADATA}" ${LAYERS}"#, r#""${TEST_PLATFORM}""#)
    } else {
        (r#""${BUNDLE}""#, r#""${{ matrix.platform }}""#)
    };

    // `set -u` is in force, so this line may only be emitted where `BUNDLE`
    // exists — an env leg never sets it.
    let metadata_sibling = if is_env {
        ""
    } else {
        "            METADATA_SIBLING=\"${BUNDLE%.tar.xz}-metadata.json\"\n"
    };

    // Emitted only when some leg declares an image, so native-only workflows
    // stay byte-identical to the pre-container renderer. `{OCX_TEST}` is the
    // command prefix: the `ocx_test` wrapper under container mode, plain `ocx`
    // otherwise.
    let has_container = legs.iter().any(|leg| !leg.container_image.is_empty());
    let (container_prelude, ocx_test) = if has_container {
        (
            r#"            # Container legs: fetch a libc-matched static ocx once, then run every
            # `ocx package test` inside `docker run <image>` so the mirrored
            # artifact is executed by that image's loader. Native legs (empty
            # container_image, e.g. the macOS/Windows legs of a mixed spec) call
            # the runner's own ocx unchanged.
            CONTAINER_IMAGE="${{ matrix.container_image }}"
            if [ -n "${CONTAINER_IMAGE}" ]; then
              # `docker_platform` is `platform` minus any `os.features` suffix:
              # docker rejects `linux/amd64+libc.musl` outright, while the matrix
              # label, the `ocx package test --platform` flag and the discover
              # platform set all need the full, unambiguous key.
              case "${{ matrix.docker_platform }}" in
                linux/amd64) OCX_ARCH=x86_64 ;;
                linux/arm64) OCX_ARCH=aarch64 ;;
                *) echo "::error::no static ocx release for container platform ${{ matrix.docker_platform }} (linux/amd64 and linux/arm64 only)"; exit 1 ;;
              esac
              # Container legs run the artifact natively — no qemu is installed,
              # so a leg whose runner is a different architecture cannot execute
              # the image at all. Say that, rather than let docker fail with a
              # bare exec-format error several minutes in.
              RUNNER_ARCH_UNAME="$(uname -m)"
              if [ "${RUNNER_ARCH_UNAME}" != "${OCX_ARCH}" ]; then
                echo "::error::container legs for ${{ matrix.docker_platform }} need a ${OCX_ARCH} runner (this one is ${RUNNER_ARCH_UNAME}); set an arch-matched \`runner:\` on this platform"
                exit 1
              fi
              OCX_TRIPLE="${OCX_ARCH}-unknown-linux-${{ matrix.container_libc }}"
              OCX_CONTAINER_DIR="${RUNNER_TEMP}/ocx-${OCX_TRIPLE}"
              OCX_CONTAINER_BIN="${OCX_CONTAINER_DIR}/ocx"
              if [ ! -x "${OCX_CONTAINER_BIN}" ]; then
                mkdir -p "${OCX_CONTAINER_DIR}"
                # cargo-dist archives carry a single top-level directory, so
                # --strip-components=1 lands the binary directly. A layout change
                # fails the chmod instead of silently testing nothing.
                curl -fsSL "https://github.com/ocx-sh/ocx/releases/download/{OCX_CLI_TAG}/ocx-${OCX_TRIPLE}.tar.gz" \
                  | tar -xz -C "${OCX_CONTAINER_DIR}" --strip-components=1
                chmod +x "${OCX_CONTAINER_BIN}"
              fi
              # Pull the image here rather than letting `docker run` do it
              # implicitly: a rate-limited or flaky pull inside the test loop is
              # recorded as a failed testcase, indistinguishable from a mirrored
              # artifact that genuinely does not run. Failing this step instead
              # keeps a red testcase meaning exactly one thing. Guarded by
              # `inspect` — that is when `docker run` would have pulled anyway,
              # and a pull per version would spend a manifest request (the thing
              # being rate-limited) on every one. It also precedes the setup
              # build below, so a provisioned image's FROM resolves from cache.
              if ! docker image inspect "${CONTAINER_IMAGE}" >/dev/null 2>&1; then
                OCX_PULL_ATTEMPT=1
                OCX_PULL_DELAY=2
                until docker pull --platform "${{ matrix.docker_platform }}" "${CONTAINER_IMAGE}"; do
                  if [ "${OCX_PULL_ATTEMPT}" -ge 5 ]; then
                    echo "::error::could not pull ${CONTAINER_IMAGE} for ${{ matrix.docker_platform }} after ${OCX_PULL_ATTEMPT} attempts (rate limit, network, or a tag with no ${{ matrix.docker_platform }} variant — see the docker output above)"
                    exit 1
                  fi
                  echo "pull of ${CONTAINER_IMAGE} failed (attempt ${OCX_PULL_ATTEMPT}); retrying in ${OCX_PULL_DELAY}s"
                  sleep "${OCX_PULL_DELAY}"
                  OCX_PULL_ATTEMPT=$((OCX_PULL_ATTEMPT + 1))
                  OCX_PULL_DELAY=$((OCX_PULL_DELAY * 2))
                done
              fi
{CONTAINER_SETUP_BUILD}            fi
            ocx_test() {
              if [ -n "${CONTAINER_IMAGE}" ]; then
                # The workspace is mounted at its own path so the bundle and its
                # `-metadata.json` sibling resolve identically inside and out.
                # OCX_HOME points into the container's own /tmp, so the install
                # the test runs against never touches the runner's filesystem.
                # The gnu ocx verifies TLS against the system CA store, which a
                # minimal base image need not carry; the musl build has webpki
                # roots baked in and ignores the mount.
                docker run --rm -i --platform "${{ matrix.docker_platform }}" \
                  -v "${GITHUB_WORKSPACE}:${GITHUB_WORKSPACE}" -w "${GITHUB_WORKSPACE}" \
                  -v "${OCX_CONTAINER_BIN}:/usr/local/bin/ocx:ro" \
                  -v /etc/ssl/certs/ca-certificates.crt:/etc/ssl/certs/ca-certificates.crt:ro \
                  -e OCX_HOME=/tmp/ocx-home -e OCX_NO_UPDATE_CHECK=1 \
                  "${CONTAINER_IMAGE}" ocx "$@"
              else
                ocx "$@"
              fi
            }
"#,
            "ocx_test",
        )
    } else {
        ("", "ocx")
    };

    // Provisioning sits inside the container branch, after the arch guard, so
    // an arm64-on-amd64 leg still gets its own diagnosis rather than a docker
    // exec-format error. The `inspect` guard makes the build once-per-leg: the
    // second and every later version of the run finds the tag already there.
    //
    // ponytail: the tag is runner-local and `(container_id, platform_slug)` is
    // unique per leg, which is enough even on a shared self-hosted runner. A
    // pathological `id:` can push it past docker's 128-char tag cap — that
    // fails the build loudly, upgrade to a hash if a real spec ever hits it.
    let container_setup_build = if any_container_setup(legs) {
        r#"              if [ -n "${OCX_CONTAINER_DOCKERFILE:-}" ]; then
                OCX_SETUP_TAG="ocx-mirror-setup:${{ matrix.container_id }}-${{ matrix.platform_slug }}"
                if ! docker image inspect "${OCX_SETUP_TAG}" >/dev/null 2>&1; then
                  printf '%s' "${OCX_CONTAINER_DOCKERFILE}" \
                    | docker build --platform "${{ matrix.docker_platform }}" -t "${OCX_SETUP_TAG}" - \
                    || { echo "::error::container setup failed for ${{ matrix.container_id }} on ${{ matrix.platform }}: a setup: command exited non-zero (the failing RUN is above)"; exit 1; }
                fi
                CONTAINER_IMAGE="${OCX_SETUP_TAG}"
              fi
"#
    } else {
        ""
    };

    let body = r#"{CONTAINER_PRELUDE}{METADATA_SIBLING}            mkdir -p junit
            JUNIT_FILE="junit/junit-${VERSION}-${{ matrix.platform_slug }}-${{ matrix.container_id }}.xml"
            TESTS_JSON='${{ toJson(matrix.tests) }}'
            TEST_COUNT=$(echo "${TESTS_JSON}" | jq 'length' | tr -d '\r')
            FAILURES=0
            CASES=""
            for i in $(seq 0 $((TEST_COUNT - 1))); do
              TEST_NAME=$(echo "${TESTS_JSON}" | jq -r ".[$i].name" | tr -d '\r')
              TEST_KIND=$(echo "${TESTS_JSON}" | jq -r ".[$i].kind" | tr -d '\r')
              START=$(date +%s)
              RC=0
              if [ "${TEST_KIND}" = "command" ]; then
                TEST_CMD=$(echo "${TESTS_JSON}" | jq -r ".[$i].command" | tr -d '\r')
                {OCX_TEST} package test --platform {TEST_PLATFORM} --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} -- \
                  ${{ matrix.shell }} -c "${TEST_CMD}" || RC=$?
              elif [ "${TEST_KIND}" = "script" ]; then
                TEST_SCRIPT=$(echo "${TESTS_JSON}" | jq -r ".[$i].script" | tr -d '\r')
                {OCX_TEST} package test --platform {TEST_PLATFORM} --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} \
                  --script "${TEST_SCRIPT}" || RC=$?
              else
                TEST_INLINE=$(echo "${TESTS_JSON}" | jq -r ".[$i].script_inline" | tr -d '\r')
                printf '%s' "${TEST_INLINE}" | {OCX_TEST} package test --platform {TEST_PLATFORM} --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} \
                  --script - || RC=$?
              fi
              END=$(date +%s)
              DUR=$((END - START))
              if [ "${RC}" -eq 0 ]; then
                CASES="${CASES}    <testcase name=\"${TEST_NAME}\" classname=\"${VERSION}.${{ matrix.platform_slug }}.${{ matrix.container_id }}\" time=\"${DUR}\"/>\n"
              else
                CASES="${CASES}    <testcase name=\"${TEST_NAME}\" classname=\"${VERSION}.${{ matrix.platform_slug }}.${{ matrix.container_id }}\" time=\"${DUR}\"><failure type=\"NonZeroExit\" message=\"exit ${RC}\"/></testcase>\n"
                FAILURES=$((FAILURES + 1))
              fi
            done
            {
              echo '<?xml version="1.0" encoding="UTF-8"?>'
              echo "<testsuites>"
              echo "  <testsuite name=\"${VERSION}.${{ matrix.platform_slug }}.${{ matrix.container_id }}\" tests=\"${TEST_COUNT}\" failures=\"${FAILURES}\">"
              if [ -n "${CI_JOB_URL:-}" ]; then
                echo "    <properties>"
                echo "      <property name=\"ci.job.url\" value=\"${CI_JOB_URL}\"/>"
                echo "    </properties>"
              fi
              printf '%b' "${CASES}"
              echo "  </testsuite>"
              echo "</testsuites>"
            } > "${JUNIT_FILE}"
            echo "wrote ${JUNIT_FILE} (tests=${TEST_COUNT}, failures=${FAILURES})"
            if [ "${FAILURES}" -gt 0 ]; then
              exit 1
            fi
"#;
    body.replace("{CONTAINER_PRELUDE}", container_prelude)
        // After `{CONTAINER_PRELUDE}` — the placeholder lives inside it.
        .replace("{CONTAINER_SETUP_BUILD}", container_setup_build)
        .replace("{METADATA_SIBLING}", metadata_sibling)
        .replace("{OCX_TEST}", ocx_test)
        .replace("{TEST_TARGET}", test_target)
        .replace("{TEST_PLATFORM}", test_platform)
}

/// The `discover` job's plan-artifact `path:`, source-dependent.
///
/// A `pypi` source derives one PEP 751 lock per discovered version during the
/// plan phase (`pipeline plan` writes them to `./locks`), and `prepare` needs
/// those locks as much as it needs `plan.json` — carrying both in the one
/// artifact is what keeps a prepare leg from re-deriving. Every other source,
/// `pylock` included (its lock is committed in the repository), uploads the
/// single file it always did.
fn plan_artifact_path(is_pypi: bool) -> &'static str {
    if is_pypi {
        "|\n            plan.json\n            locks/"
    } else {
        "plan.json"
    }
}

/// The `actions/upload-artifact` step header exactly as the template pins it.
///
/// [`derived_locks_artifact`] is a step the template cannot carry — it exists
/// for one source type only — and a pin written into Rust would sit outside the
/// Renovate customManager, which scans `templates/*.yml`. Reading the
/// template's own line keeps both uploads on the one bot-bumped action. The
/// fallback is inert while the template carries an upload step, which
/// `the_derived_locks_upload_tracks_the_templates_action_pin` asserts.
fn upload_artifact_uses() -> &'static str {
    WORKFLOW_TEMPLATE
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("- uses: actions/upload-artifact@"))
        .unwrap_or("- uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1")
}

/// The long-retention copy of a `pypi` source's derived locks, or nothing.
///
/// The `plan` artifact expires after a day, and the locks it carries are the
/// exact resolution every published env package was built from — one 90-day
/// copy is what makes a past publish reconstructable
/// (`adr_pypi_lock_derivation.md`). `if-no-files-found: ignore` because a run
/// that discovers no new version derives no lock and must not red on the empty
/// directory.
fn derived_locks_artifact(is_pypi: bool) -> String {
    if !is_pypi {
        return String::new();
    }
    format!(
        r#"      # Long-retention copy of the derived locks, for audit once the 1-day
      # plan artifact has expired (see adr_pypi_lock_derivation.md).
      {uses}
        with:
          name: derived-locks
          path: locks/
          retention-days: 90
          if-no-files-found: ignore
"#,
        uses = upload_artifact_uses(),
    )
}

/// The prepare job's artifact-gathering script (10-space indent, emitted into
/// that job's `run:` block).
///
/// Archive legs flatten every per-platform `bundle.tar.xz` and its metadata
/// sibling into one `bundles/` namespace keyed by `bundle-{V}-{slug}`. An env
/// source has no such file: `pipeline prepare` writes a version subtree whose
/// `env-manifest.json` names its metadata and layers by paths relative to that
/// directory, so the subtree travels whole and both the test job and
/// `pipeline push`'s `enumerate_env_manifests` resolve those paths against
/// `bundles/{V}/`.
fn prepare_flatten_script(is_env: bool) -> &'static str {
    if is_env {
        r#"          # The prepared env subtree travels whole: env-manifest.json names its
          # metadata and layers relative to this version directory.
          V="${{ matrix.version.version }}"
          # Same `+` → `_` translation the archive path makes: `pipeline prepare`
          # names its on-disk version directory with the OCI-tag-safe slug while
          # the matrix value keeps the build separator.
          V_SLUG="${V//+/_}"
          mkdir -p bundles
          if [ -d ".ocx-mirror/${V_SLUG}" ]; then
            cp -R ".ocx-mirror/${V_SLUG}" "bundles/${V}"
          fi"#
    } else {
        r#"          # Flatten .ocx-mirror/{V}/{P}/bundle.tar.xz → bundles/bundle-{V}-{P_slug}.tar.xz
          # and copy the per-platform metadata.json written by `pipeline prepare`
          # as sibling so `ocx package test` auto-discovers the correct override
          # (e.g. metadata-darwin.json baked content) via its bundle→metadata
          # sibling convention. Do NOT copy the spec-level metadata.json from CWD
          # — that always contains the default path, not the platform override.
          V="${{ matrix.version.version }}"
          # `pipeline prepare` normalises the build separator `+` → `_` when
          # naming its on-disk version directory (OCI-tag safe slug); the
          # matrix value still carries the original `+`, so translate before
          # globbing into the platform tree.
          V_SLUG="${V//+/_}"
          mkdir -p bundles
          shopt -s nullglob
          for platform_dir in ".ocx-mirror/${V_SLUG}"/*/; do
            [ -d "${platform_dir}" ] || continue
            P_SLUG=$(basename "${platform_dir}")
            cp "${platform_dir}bundle.tar.xz" "bundles/bundle-${V}-${P_SLUG}.tar.xz"
            cp "${platform_dir}metadata.json" "bundles/bundle-${V}-${P_SLUG}-metadata.json"
          done"#
    }
}

/// Resolve this leg's package under test, per version (12-space indent, emitted
/// immediately before the test-run steps inside the test job's version loop).
///
/// Archive legs name one bundle path. Env legs read the version's
/// `env-manifest.json` and set `METADATA` + `LAYERS` for the
/// `-m <metadata> <layers…>` form, picking the entry that matches this leg's
/// libc: a musl container leg tests the musl env of a dual-libc package, while
/// both legs of a single-env package see its one featureless entry.
///
/// The jq resolution is deliberately guarded (`2>/dev/null || true`, `// empty`)
/// so a genuine miss — a version whose prepare leg failed and uploaded no
/// manifest — reds that one version attributably through the
/// `ocx package test … || RC=$?` capture (empty METADATA → ocx fails → one JUnit
/// `<failure>`), instead of a bare jq exit tripping `set -e` and aborting every
/// remaining version with no JUnit written at all.
///
/// Every jq capture ends in `tr -d '\r'`: on windows-latest the captured value
/// otherwise keeps a trailing CR (Git Bash word-splits `$()` on LF only), which
/// reaches `ocx package test` as part of the path and fails it with os error
/// 123. `| tr -d '\r'` sits inside the pipeline so `|| true` still swallows a
/// genuine miss — `pipefail` propagates jq's exit through it unchanged.
fn test_target_resolve_script(is_env: bool) -> &'static str {
    if is_env {
        r#"            VERSION_DIR="bundles/${VERSION}"
            # The leg's own libc, declared on `--platform` as an os_feature so
            # dependency resolution (the env's private interpreter) can select a
            # per-libc index entry. Native legs declare no libc and stay bare.
            TEST_PLATFORM="${{ matrix.platform }}"
            case "${TEST_PLATFORM}" in
              # A platform key that already declares its own os.features is
              # authoritative — appending a second one would not even parse.
              *+*) ;;
              *)
                case "${{ matrix.container_libc }}" in
                  musl) TEST_PLATFORM="${TEST_PLATFORM}+libc.musl" ;;
                  gnu) TEST_PLATFORM="${TEST_PLATFORM}+libc.glibc" ;;
                esac
                ;;
            esac
            ENV_JSON=$(jq -c --arg p "${{ matrix.platform }}" --arg full "${TEST_PLATFORM}" '([.envs[] | select(.platform == $full)] + [.envs[] | select(.platform == $p)]) | first // empty' "${VERSION_DIR}/env-manifest.json" 2>/dev/null | tr -d '\r' || true)
            METADATA="${VERSION_DIR}/$(printf '%s' "${ENV_JSON}" | jq -r '.metadata_path // empty' 2>/dev/null | tr -d '\r' || true)"
            LAYERS=""
            for rel in $(printf '%s' "${ENV_JSON}" | jq -r '.layers[].path // empty' 2>/dev/null | tr -d '\r' || true); do
              LAYERS="${LAYERS} ${VERSION_DIR}/${rel}"
            done"#
    } else {
        r#"            BUNDLE="bundles/bundle-${VERSION}-${{ matrix.platform_slug }}.tar.xz""#
    }
}

/// Render the describe.yml catalog-publish workflow.
///
/// Lighter than `mirror.yml`: only the auth + target-registry placeholders need
/// substitution. The workflow itself triggers on changes to `CATALOG.md`,
/// `logo.*`, or `mirror.yml` and invokes
/// `ocx-mirror package pipeline describe` to publish the README + logo to the
/// `__ocx.desc` referrer tag on the target repository.
fn render_describe(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    let triggers = trigger_paths(
        slot,
        &["CATALOG.md".to_string(), "logo.*".to_string(), slot.source()],
        "describe",
    );

    DESCRIBE_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace("{TRIGGER_PATHS}", &triggers)
        .replace("{DESCRIBE_PERMISSIONS}", render_registry_write_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{TARGET_REGISTRY}", &spec.target.registry)
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// Render the `announce-from-registry.yml` catch-up workflow.
///
/// Same placeholder set as `describe.yml` — auth steps plus a GHCR permissions
/// block. Dispatch is always available and defaults to reporting: the push job
/// already announces what each run publishes, and this one exists for the
/// backlog a mirror that opted into `announce:` late can never reach by running
/// forward. `announce: { schedule: … }` adds a `schedule:` trigger whose runs
/// announce for real; a run that finds nothing new commits nothing, and opens a
/// pull request only for commits an earlier run stranded on the announce branch
/// (see `AnnounceReport` in `pipeline/push.rs`).
///
/// Keeps a concurrency group of its own rather than joining the push workflow's
/// the way `cascade.yml` does — see the template's comment on the group.
///
/// Takes **discover's** read-only permissions, not describe's. This job only
/// lists the target's tags and fetches their manifests — the writes it performs
/// all land on the index repository, through `OCX_ANNOUNCE_TOKEN`, never through
/// the job's own `GITHUB_TOKEN`. Handing it `packages: write` would grant the
/// one scope that could overwrite the very artifacts it exists to describe.
fn render_announce_from_registry(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    ANNOUNCE_FROM_REGISTRY_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace(
            "{ANNOUNCE_SCHEDULE_BLOCK}",
            &schedule_block(spec.announce.as_ref().and_then(|a| a.schedule.as_ref())),
        )
        .replace("{ANNOUNCE_PERMISSIONS}", render_discover_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// Render the `patch.yml` metadata-correction workflow.
///
/// Dispatch-only, and deliberately not wired to `pipeline plan`'s `has_drift`
/// output: a drift finding says the published metadata no longer matches the
/// spec, and whether that is worth re-emitting every affected manifest is a
/// maintainer's call. The point of generating it at all is that patching
/// otherwise needs registry push credentials and an index token on somebody's
/// laptop, which is not how any other pipeline verb is run.
///
/// The three `workflow_dispatch` inputs are the command's whole selection
/// surface; the run step turns each present one into its flag and each absent
/// one into nothing, so an empty dispatch patches every published version.
///
/// Takes the same `packages: write` block describe does — this job re-emits
/// manifests on the target repository. The announce it chains into writes to
/// the index repository with `OCX_ANNOUNCE_TOKEN`.
fn render_patch(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    PATCH_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace("{PATCH_PERMISSIONS}", render_registry_write_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// The workflow-level `concurrency:` group of *this spec's* push workflow.
///
/// `workflow.yml` spells it `mirror-${{ github.workflow }}-publish`, and
/// `github.workflow` is that workflow's `name:` — which the renderer sets to
/// `spec.name`. Resolving it here lets another workflow name the same group
/// without a runtime handle on the push workflow.
fn publish_concurrency_group(spec: &MirrorSpec) -> String {
    format!("mirror-{}-publish", spec.name)
}

/// A workflow's `schedule:` trigger, or nothing.
///
/// The templates place the placeholder on the line above `workflow_dispatch:`,
/// so an absent cron collapses to no lines at all.
///
/// The cron lands inside a single-quoted scalar unescaped; what keeps a spec
/// from closing it and appending triggers of its own is `spec::validate_cron`,
/// which every caller's spec passes through before any file is written.
fn schedule_block(cron: Option<&String>) -> String {
    cron.map(|cron| format!("  schedule:\n    - cron: '{}'\n", cron))
        .unwrap_or_default()
}

/// Render the `cascade.yml` rolling-tag repair workflow.
///
/// Dispatch is always available and defaults to `dry_run: true`, so a repair
/// that nobody asked for in writing only audits. `cascade: { schedule: … }`
/// adds a `schedule:` trigger whose runs repair for real. Emitted only for a
/// spec that cascades — a mirror publishing no rolling alias has no graph to
/// repair.
///
/// Shares the push workflow's concurrency group (see
/// [`publish_concurrency_group`]) so a repair never runs while a publish is
/// mid-way through writing the same aliases. GitHub holds one pending run per
/// group, so the trade is that a run *waiting* in that group — a repair or a
/// publish — is cancelled when a newer run of either workflow queues.
///
/// Takes the same `packages: write` block describe and patch do: a repair
/// writes tags to the target repository. The announce it chains into writes to
/// the index repository with `OCX_ANNOUNCE_TOKEN`.
fn render_cascade(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    CASCADE_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace(
            "{CASCADE_SCHEDULE_BLOCK}",
            &schedule_block(spec.cascade.schedule.as_ref()),
        )
        .replace("{PUSH_CONCURRENCY_GROUP}", &publish_concurrency_group(spec))
        .replace("{CASCADE_PERMISSIONS}", render_registry_write_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// The `--spec` arguments the drift guard re-renders the repository with.
///
/// Empty for the lone repo-root `mirror.yml`, whose path is what `--spec`
/// already defaults to — that is what keeps every published mirror's committed
/// guard byte-identical. As soon as the repository holds a spec the default
/// cannot name, *every* spec is listed: `--spec` appends, so naming one would
/// silently drop the rest and the guard would check a subset while looking green.
fn verify_spec_args(slots: &[&SpecSlot]) -> String {
    if slots.iter().all(|slot| slot.is_default()) {
        return String::new();
    }
    slots.iter().map(|slot| format!(" --spec {}", slot.source())).collect()
}

/// Render the `verify-generated.yml` drift-guard workflow.
///
/// The workflow runs `ocx-mirror package pipeline generate ci --check` on pull requests
/// and pushes, so a hand-edit to any generated workflow fails CI. Exactly one is
/// emitted per repository — it names every spec, and its path triggers are the
/// union of theirs, which makes the committed file the record of what the
/// repository mirrors. Emitted unless *every* spec opts out via
/// `allow_manual_edits` (see [`render`]).
fn render_verify_generated(slots: &[&SpecSlot]) -> String {
    let mut entries = Vec::new();
    for slot in slots {
        match slot.dir() {
            None => entries.extend([
                slot.source(),
                "scripts/**".to_string(),
                "tests/**".to_string(),
                "metadata*.json".to_string(),
            ]),
            Some(dir) => entries.push(format!("{}/**", slash_path(dir))),
        }
        entries.extend(slot.extends_entries());
    }
    entries.push(".github/workflows/**".to_string());
    // Siblings share one base, so the same path arrives once per child. Keep the
    // first occurrence: order carries which spec brought an entry in.
    let mut seen = std::collections::HashSet::new();
    entries.retain(|entry| seen.insert(entry.clone()));

    let sources = slots.iter().map(|slot| slot.source()).collect::<Vec<_>>().join(", ");

    VERIFY_GENERATED_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &sources)
        .replace("{SPEC_ARGS}", &verify_spec_args(slots))
        .replace("{TRIGGER_PATHS}", &indent_entries(&entries))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
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

/// Write all rendered files to disk under `repo_root`.
///
/// Creates parent directories as needed. Uses `tokio::fs::write` which is
/// atomic from the caller's perspective (single write call per file).
async fn write_files(files: &BTreeMap<PathBuf, String>, repo_root: &Path) -> Result<(), MirrorError> {
    for (relative_path, content) in files {
        let dest = repo_root.join(relative_path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                MirrorError::TemplateError(format!("failed to create directory {}: {e}", parent.display()))
            })?;
        }
        tokio::fs::write(&dest, content)
            .await
            .map_err(|e| MirrorError::TemplateError(format!("failed to write {}: {e}", dest.display())))?;
    }
    Ok(())
}

// ── Drift detector ────────────────────────────────────────────────────────────

/// Matches a `uses:` action-reference line so its *pin* can be normalized away
/// before drift comparison. Group `keep` holds the `uses: owner/action` head; the
/// `@<ref>` is matched only when `<ref>` is **pin-shaped** — a single run of
/// non-space, non-`#` characters (a digest or tag) optionally followed by a
/// `# vX` version comment — and that suffix is dropped. A `uses:` line carrying
/// anything else after the ref (shell metacharacters, a second token, inline
/// `with:`-like text) does NOT match, so such a hand-edit still trips drift
/// rather than being masked. Per-line anchored (`(?m)`); matches both `- uses:`
/// list items and bare `uses:` step keys.
///
/// The regex is line-oriented and YAML-unaware: it would also match an indented
/// `uses: owner/action@<ref>` line *inside* a `run:` block scalar. No current
/// template emits such a line — keep it that way (do not emit `run:` block lines
/// beginning with `uses: …@…`) or the pin on that script line would be masked.
static USES_REF_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?m)^(?P<keep>[ \t]*(?:-[ \t]+)?uses:[ \t]*[^@\s]+)@[^\s#]+(?:[ \t]*#[^\n]*)?$").unwrap()
});

/// Canonicalize a generated workflow for drift comparison.
///
/// Mirror repositories own the *pin* on every `uses:` action reference: their
/// own Renovate/Dependabot may bump `uses: owner/action@<ref>  # vX` and the
/// drift guard must not treat that as a hand-edit. The guard still polices the
/// workflow *logic* and *which* action each step runs — only the `@<ref>` suffix
/// (digest or tag, plus any trailing version comment) is stripped; the
/// `owner/action` identity is preserved, so swapping in a different action still
/// trips drift. The baked template ships a known-good seed pin for first render.
fn normalize_for_drift(content: &str) -> std::borrow::Cow<'_, str> {
    USES_REF_RE.replace_all(content, "${keep}")
}

/// First line of every file this renderer writes.
const GENERATED_HEADER: &str = "# Generated by ocx-mirror";

/// Generated workflows on disk that the current spec set no longer renders.
///
/// Dropping a spec leaves its workflows behind, and a mirror workflow whose
/// spec no longer exists keeps running on schedule against a spec that is gone.
/// Only files carrying the generated header are considered, so a repository's
/// hand-written workflows are never touched.
async fn stale_generated(files: &BTreeMap<PathBuf, String>, repo_root: &Path) -> Vec<String> {
    let workflows_dir = repo_root.join(".github/workflows");
    let Ok(mut entries) = tokio::fs::read_dir(&workflows_dir).await else {
        return Vec::new();
    };

    let mut stale = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "yml" || ext == "yaml") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(repo_root) else {
            continue;
        };
        if files.contains_key(relative) {
            continue;
        }
        if tokio::fs::read_to_string(&path)
            .await
            .is_ok_and(|content| content.starts_with(GENERATED_HEADER))
        {
            stale.push(slash_path(relative));
        }
    }
    stale.sort();
    stale
}

/// Compare the expected generated files against what is on disk.
///
/// Returns `RendererDrift` if any file is missing, has different content — after
/// normalizing `uses:` action pins on both sides (see [`normalize_for_drift`]) —
/// or is a generated workflow no spec renders any more. Drift hints are
/// path-only — never expose file contents to stderr (secret-hygiene rule R3).
async fn check_drift(files: &BTreeMap<PathBuf, String>, repo_root: &Path) -> Result<(), MirrorError> {
    let mut drifted: Vec<String> = Vec::new();

    for (relative_path, expected) in files {
        let on_disk_path = repo_root.join(relative_path);
        match tokio::fs::read_to_string(&on_disk_path).await {
            Ok(actual) => {
                if normalize_for_drift(&actual) != normalize_for_drift(expected) {
                    // Emit path-only hint; content never printed (R3).
                    eprintln!("drift: {}", relative_path.display());
                    drifted.push(relative_path.display().to_string());
                }
            }
            Err(_) => {
                // Missing file counts as drift.
                eprintln!("drift: {}", relative_path.display());
                drifted.push(relative_path.display().to_string());
            }
        }
    }

    for path in stale_generated(files, repo_root).await {
        eprintln!("stale: {path}");
        drifted.push(path);
    }

    if drifted.is_empty() {
        Ok(())
    } else {
        Err(MirrorError::RendererDrift(drifted))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "ci/tests.rs"]
mod tests;
