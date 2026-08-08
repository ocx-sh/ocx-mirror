// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline prepare` — download, verify, and bundle one version
//! across all declared platforms. Mirrors the per-version subset of the
//! existing `command/sync.rs` Phase-1 loop.

use std::path::PathBuf;

use ocx_lib::cli::DataInterface;
use ocx_lib::log;

use crate::command::package::pipeline::plan::{
    PlanReport, PlanVersionEntry, derive_one_pypi_lock, derived_lock_filename, pylock_interpreter_pin,
    pylock_target_platform, resolve_uv_python, wheel_target_constraints,
};
use crate::command::package::sync::list_upstream_versions;
use crate::error::MirrorError;
use crate::normalizer;
use crate::pipeline::mirror_task::{MirrorTask, VariantContext};
use crate::pipeline::orchestrator::{self, ConcurrencyParams};
use crate::pipeline::python_prepare::{self, SelectedWheel, WheelEnvTask};
use crate::resolver;
use crate::resolver::asset_resolution::AssetResolution;
use crate::spec::{self, MirrorSpec};

/// `ocx-mirror package pipeline prepare` subcommand.
///
/// Outputs `{work_dir}/{V}/{platform_slug}/bundle.tar.xz` per declared
/// platform and `{work_dir}/{V}/manifest.json` listing bundles with sizes
/// and digests.
#[derive(clap::Parser)]
pub struct Prepare {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Version to prepare (e.g. `3.29.0`).
    #[arg(long, required = true)]
    pub version: String,

    /// Working directory for intermediate artifacts. Defaults to `./.ocx-mirror`.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// Path to a `plan.json` produced by `pipeline plan`. When set, tasks are
    /// built from the plan's resolved assets and the source is never queried —
    /// one crawl per pipeline run instead of one per prepare leg (issue #160).
    #[arg(long)]
    pub plan: Option<PathBuf>,
}

impl Prepare {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        let spec_path = &self.spec;
        let spec = spec::load_spec(spec_path).await?;
        let spec_dir = spec_path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();

        let work_dir = self
            .work_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".ocx-mirror"));

        // Env-package sources (`pylock`, `pypi`) take a parallel env-prepare
        // path: wheels are re-selected from a lock (committed for `pylock`,
        // derived in-pipeline for `pypi`) and composed into env packages. The
        // archive/binary path below is untouched.
        if spec.source.is_env() {
            return self.execute_pylock(&spec, &spec_dir, &work_dir).await;
        }

        let tasks = match &self.plan {
            Some(plan_path) => {
                let plan = read_plan(plan_path).await?;
                build_tasks_from_plan(&spec, &spec_dir, &plan, &self.version)?
            }
            None => build_tasks_for_version(&spec, &spec_dir, &self.version).await?,
        };

        if tasks.is_empty() {
            return Err(MirrorError::SpecInvalid(vec![format!(
                "version '{}' not found in upstream source or no platforms resolved",
                self.version
            )]));
        }

        log::info!(
            "[{}] Preparing version {} ({} platforms)",
            spec.name,
            self.version,
            tasks.len()
        );

        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to create work dir: {e}")]))?;

        let http_client = reqwest::Client::new();
        let concurrency = ConcurrencyParams {
            max_downloads: spec.concurrency.max_downloads,
            max_bundles: spec.concurrency.max_bundles,
            compression_threads: spec::resolve_compression_threads(
                spec.concurrency.compression_threads,
                spec.concurrency.max_bundles,
            ),
        };

        let manifest =
            orchestrator::prepare_version(&self.version, &tasks, &work_dir, &http_client, &concurrency).await?;

        let manifest_path = work_dir.join(&self.version).join("manifest.json");
        println!("{}", manifest_path.display());

        log::debug!(
            "[{}] Prepared {} bundles for version {}",
            spec.name,
            manifest.bundles.len(),
            self.version
        );

        Ok(())
    }

    /// Env-prepare path for `source.type: pylock`/`pypi` specs — the parallel
    /// to the archive/binary path in [`execute`](Self::execute).
    ///
    /// Builds one env task per applicable `wheels:` key of the requested
    /// version, then downloads + repacks + composes them into
    /// `{work_dir}/{version}/env-manifest.json`.
    async fn execute_pylock(
        &self,
        spec: &MirrorSpec,
        spec_dir: &std::path::Path,
        work_dir: &std::path::Path,
    ) -> Result<(), MirrorError> {
        let client =
            ocx_lib::oci::ClientBuilder::from_env().map_err(|e| MirrorError::ExecutionFailed(vec![e.to_string()]))?;
        let python = spec.python.as_ref().ok_or_else(|| {
            MirrorError::SpecInvalid(vec![
                "python config is required for source.type 'pylock'/'pypi'".to_string(),
            ])
        })?;
        // The interpreter candidate fetch is the one network dependency of
        // task building; fetching the tag's per-platform leaf candidates here
        // keeps `build_env_tasks` a pure (hermetically testable) local
        // re-selection — each leg then pins its own platform LEAF manifest
        // digest locally. One spec-wide `python.interpreter_package` — no
        // per-key override.
        let interpreter_candidates = fetch_interpreter_candidates(&python.interpreter_package, &client).await?;

        // When `--plan` is supplied (the CI path), restrict prepare to the
        // wheels keys discover still needs for this version. discover emits a
        // backfill-partial entry that lists only the outstanding work, so an
        // already-published tile is not re-composed (and not later false-red at
        // push for a missing JUnit). The allowed set is the FULL key strings
        // from the entry's resolved assets (they carry `+libc.*` suffixes;
        // `entry.platforms` holds only deduped base os/arch strings), falling
        // back to `entry.platforms` for an assets-less legacy plan. Without a
        // plan (standalone prepare), fall back to every applicable wheels key.
        //
        // The entry is looked up under the same either-form contract the task
        // builder below applies to `--version` ([`plan_entry_for_version`]) —
        // matching only the entry's stamped tag would silently yield an EMPTY
        // allowed set for a bare `--version`, and prepare would then compose
        // nothing at all.
        let allowed_platforms: Option<std::collections::HashSet<String>> = match &self.plan {
            Some(plan_path) => {
                let plan = read_plan(plan_path).await?;
                Some(
                    plan_entry_for_version(&plan, &self.version)
                        .map(|entry| {
                            if entry.assets.is_empty() {
                                entry.platforms.iter().cloned().collect()
                            } else {
                                entry.assets.iter().map(|asset| asset.platform.clone()).collect()
                            }
                        })
                        .unwrap_or_default(),
                )
            }
            None => None,
        };

        // `pypi` sources need their own task-building path (a plan-supplied
        // derived lock to consume, or a from-scratch re-derivation when
        // running standalone) — kept as a sibling function rather than
        // widening `build_env_tasks`'s signature, so its existing
        // committed-lock-only test suite stays untouched.
        let tasks = match &spec.source {
            spec::Source::Pypi { .. } => {
                build_pypi_env_tasks(
                    spec,
                    spec_dir,
                    &self.version,
                    &interpreter_candidates,
                    allowed_platforms.as_ref(),
                    self.plan.as_deref(),
                    work_dir,
                )
                .await?
            }
            _ => {
                build_env_tasks(
                    spec,
                    spec_dir,
                    &self.version,
                    &interpreter_candidates,
                    allowed_platforms.as_ref(),
                )
                .await?
            }
        };

        if tasks.is_empty() {
            return Err(MirrorError::SpecInvalid(vec![format!(
                "version '{}' not found in pylock source or no platforms resolved",
                self.version
            )]));
        }

        // The version directory is named by the PUBLISHED tag, which the task
        // builder resolved (`--version` may have named the bare release
        // instead). It must be the one `task_dir` uses, or the manifest's
        // paths do not sit under the directory it is written in and
        // `enumerate_env_manifests` refuses them as out-of-tree.
        let published_tag = tasks[0].normalized_version.clone();

        log::info!(
            "[{}] Preparing pylock env version {} ({} platforms)",
            spec.name,
            published_tag,
            tasks.len()
        );

        tokio::fs::create_dir_all(work_dir)
            .await
            .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to create work dir: {e}")]))?;

        let http_client = reqwest::Client::new();
        let concurrency = ConcurrencyParams {
            max_downloads: spec.concurrency.max_downloads,
            max_bundles: spec.concurrency.max_bundles,
            compression_threads: spec::resolve_compression_threads(
                spec.concurrency.compression_threads,
                spec.concurrency.max_bundles,
            ),
        };

        let manifest =
            python_prepare::prepare_env_version(&published_tag, &tasks, work_dir, &http_client, &concurrency).await?;

        let manifest_path = work_dir.join(&published_tag).join("env-manifest.json");
        println!("{}", manifest_path.display());

        log::debug!(
            "[{}] Prepared {} env packages for version {}",
            spec.name,
            manifest.envs.len(),
            published_tag
        );

        Ok(())
    }
}

/// Build [`WheelEnvTask`]s for `version` from the committed pylock.
///
/// Thin wrapper: loads the committed lock and resolves the app version, then
/// delegates task construction to the lock-agnostic [`build_env_tasks_from_lock`].
async fn build_env_tasks(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    version: &str,
    interpreter_candidates: &[(ocx_lib::oci::Identifier, ocx_lib::oci::Platform)],
    allowed_platforms: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<WheelEnvTask>, MirrorError> {
    let path = match &spec.source {
        spec::Source::Pylock { path, .. } => path,
        // `pypi` sources are handled by the sibling `build_pypi_env_tasks`
        // (its caller, `execute_pylock`, dispatches before ever reaching this
        // function for that source type) — never reached in practice, but
        // a graceful empty result rather than a panic if it ever is.
        _ => return Ok(Vec::new()),
    };

    let lock = crate::source::pylock::load(spec_dir, path)
        .await
        .map_err(|e| crate::source::pylock::classify_error("failed to load pylock source", e))?;
    let app_version = crate::source::pylock::app_version(&lock, spec.source.pylock_app_name(&spec.name))
        .map_err(|e| MirrorError::PylockError(e.to_string()))?;

    build_env_tasks_from_lock(
        spec,
        spec_dir,
        version,
        &lock,
        &app_version,
        interpreter_candidates,
        allowed_platforms,
    )
}

/// Lock-agnostic core of [`build_env_tasks`].
///
/// Pure local re-selection (no source re-crawl — issue #160): when the bare
/// env tag equals `version`, resolves a `PythonTarget` per declared,
/// applicable `wheels:` key and runs `ocx_python::select_wheels`. The private
/// interpreter's platform candidates are fetched by the caller (they need
/// the registry); the per-leg leaf pin is selected locally here.
/// Takes an already-parsed `lock`/`app_version` so it never
/// touches the filesystem — network-free and directly unit-testable.
fn build_env_tasks_from_lock(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    version: &str,
    lock: &ocx_python::Pylock,
    app_version: &str,
    interpreter_candidates: &[(ocx_lib::oci::Identifier, ocx_lib::oci::Platform)],
    allowed_platforms: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<WheelEnvTask>, MirrorError> {
    // `--version` names either the bare source version — the standalone path,
    // stamped with this run's timestamp below — or a build stamp of it, carried
    // verbatim. A leg whose requested version is neither has no work here.
    //
    // The stamped form is accepted more widely than on the archive path, not
    // identically: `build_tasks_for_version` matches only the raw upstream
    // version, this run's normalized tag, or this run's variant-prefixed tag —
    // all three recomputed from the CURRENT `build_timestamp`. Here
    // `is_build_stamp_of` accepts ANY stamp of the release, deliberately: the
    // plan and prepare run as separate jobs, so a `datetime` stamp the plan job
    // computed never equals one recomputed here, and an exact-form contract
    // would reject every planned tag.
    let published_tag = if version == app_version {
        // Standalone invocation: stamp it with this run's timestamp.
        normalizer::env_version_tag(app_version, &normalizer::build_timestamp(&spec.build_timestamp))
    } else if is_build_stamp_of(version, app_version) {
        // The plan's tag, carried verbatim. Never re-stamped here: `plan` and
        // `prepare` are separate jobs, so a `datetime` stamp recomputed now
        // would differ from the planned one by the seconds between them.
        version.to_string()
    } else {
        return Ok(Vec::new());
    };

    let python = spec
        .python
        .as_ref()
        .ok_or_else(|| MirrorError::SpecInvalid(vec!["python config is required for env sources".to_string()]))?;
    let interpreter_pin = pylock_interpreter_pin(python)?;
    let wheels_map = spec
        .wheels
        .as_ref()
        .ok_or_else(|| MirrorError::SpecInvalid(vec!["wheels config is required for env sources".to_string()]))?;

    let scope = ocx_python::WheelScope::new(spec.wheel_scope.clone());
    let declared_extras = lock.extras.clone();
    // Root = `source.package`/spec name (design decision C); resolved once per
    // version, same as `app_version` in the caller — `entrypoints:` windows
    // are resolved against this version here so `ocx_python::compose_env`
    // stays version-agnostic.
    let root_package = spec.source.pylock_app_name(&spec.name);
    let entrypoint_selection = python.resolve_entrypoint_selection(app_version, root_package);

    let mut tasks = Vec::new();
    for platform in wheels_map.sorted_platforms() {
        let key = platform.to_string();
        if !spec.platform_applies(app_version, &spec::base_platform_key(platform)) {
            continue;
        }
        // Restrict to the wheels keys the plan still needs. `discover` excludes
        // already-published tiles (a backfill-partial run adds only the new
        // keys of an existing version); without this, prepare composes the
        // already-published key too, and push then false-reds it as
        // `missing_junit` (its test leg was skipped, so it has no JUnit).
        if let Some(allowed) = allowed_platforms
            && !allowed.contains(&key)
        {
            continue;
        }

        let python_target = ocx_python::PythonTarget {
            platform: pylock_target_platform(platform, &key)?,
            variant: wheel_target_constraints(wheels_map, platform),
            interpreter: interpreter_pin.clone(),
        };

        let selected = ocx_python::select_wheels(lock, &python_target)
            .map_err(|e| MirrorError::PylockError(format!("wheel selection failed for platform '{key}': {e}")))?;

        let mut wheels = Vec::with_capacity(selected.len());
        for wheel in &selected {
            let url_str = wheel.url.as_deref().ok_or_else(|| {
                MirrorError::PylockError(format!(
                    "wheel '{}' for package '{}' selected with no download URL",
                    wheel.filename, wheel.name
                ))
            })?;
            let url = url::Url::parse(url_str)
                .map_err(|e| MirrorError::PylockError(format!("invalid wheel URL '{url_str}': {e}")))?;
            let wheel_repository = ocx_python::wheel_reference(&scope, wheel).repository;
            wheels.push(SelectedWheel {
                package_name: wheel.name.clone(),
                version: wheel.version.clone(),
                filename: wheel.filename.clone(),
                url,
                sha256: wheel.sha256.clone(),
                wheel_repository,
            });
        }

        tasks.push(WheelEnvTask {
            normalized_version: published_tag.clone(),
            source_version: app_version.to_string(),
            platform: platform.clone(),
            target: spec.target.clone(),
            cascade: spec.cascade.enabled,
            spec_dir: spec_dir.to_path_buf(),
            wheels,
            interpreter: select_interpreter_pin(&python.interpreter_package, interpreter_candidates, platform)?,
            requested_extras: Vec::new(), // W3: spec does not yet encode a per-app extras request
            declared_extras: declared_extras.clone(),
            python_target,
            wheel_scope: scope.clone(),
            entrypoint_selection: entrypoint_selection.clone(),
        });
    }

    Ok(tasks)
}

/// `source.type: pypi` env-prepare task building — the `pypi` counterpart to
/// [`build_env_tasks`] (which only reads a committed `pylock` file).
///
/// When `plan_path` resolves to a plan entry carrying a `pylock` path (the
/// lock `pipeline plan` already derived for this version), reads and parses
/// it directly — no `uv`/`ocx` subprocess needed. Otherwise (no `--plan`, or
/// a plan entry without a `pylock` path — e.g. a schema_version-1 plan)
/// re-derives the lock from scratch via the same `pipeline::lock_derive`
/// path `pipeline plan` uses ([`derive_one_pypi_lock`]), so a lone
/// `pipeline prepare` invocation still works end to end without a prior
/// `pipeline plan` run.
async fn build_pypi_env_tasks(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    version: &str,
    interpreter_candidates: &[(ocx_lib::oci::Identifier, ocx_lib::oci::Platform)],
    allowed_platforms: Option<&std::collections::HashSet<String>>,
    plan_path: Option<&std::path::Path>,
    work_dir: &std::path::Path,
) -> Result<Vec<WheelEnvTask>, MirrorError> {
    let pylock_relative = match plan_path {
        Some(path) => {
            let plan = read_plan(path).await?;
            plan_entry_for_version(&plan, version).and_then(|entry| entry.pylock.clone())
        }
        None => None,
    };

    let (lock, app_version) = match pylock_relative {
        Some(relative) => {
            // The plan carries a path relative to plan.json's own directory
            // (the same directory `--locks-dir` was written under) — resolve
            // it against `plan_path`'s parent, not `spec_dir`.
            let lock_path = plan_path
                .and_then(std::path::Path::parent)
                .unwrap_or(std::path::Path::new("."))
                .join(&relative);
            let contents = tokio::fs::read_to_string(&lock_path).await.map_err(|e| {
                MirrorError::PlanError(format!("failed to read derived lock '{}': {e}", lock_path.display()))
            })?;
            let lock = ocx_python::parse_pylock(&contents).map_err(|e| {
                MirrorError::PylockError(format!(
                    "derived lock '{}' failed to re-parse: {e}",
                    lock_path.display()
                ))
            })?;
            let app_version = crate::source::pylock::app_version(&lock, spec.source.pylock_app_name(&spec.name))
                .map_err(|e| MirrorError::PylockError(e.to_string()))?;
            (lock, app_version)
        }
        None => {
            let app_version = resolve_pypi_app_version(spec, spec_dir, version).await?;
            let python = spec.python.as_ref().ok_or_else(|| {
                MirrorError::SpecInvalid(vec!["python config is required for source.type 'pypi'".to_string()])
            })?;
            let uv_python = resolve_uv_python(python).await?;

            tokio::fs::create_dir_all(work_dir)
                .await
                .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to create work dir: {e}")]))?;
            let package = spec.source.pylock_app_name(&spec.name);
            let output_path = work_dir.join(derived_lock_filename(package, &app_version));
            let lock = derive_one_pypi_lock(spec, &uv_python, &app_version, &output_path).await?;
            (lock, app_version)
        }
    };

    build_env_tasks_from_lock(
        spec,
        spec_dir,
        version,
        &lock,
        &app_version,
        interpreter_candidates,
        allowed_platforms,
    )
}

/// The plan entry `version` names, under the same either-form contract
/// [`build_env_tasks_from_lock`] applies to `--version`: the entry's own
/// (build-stamped) publish tag, the bare source version that tag was stamped
/// from, or any other stamp of that same release.
///
/// The bare form is what a hand-run `pipeline prepare --version X.Y.Z --plan …`
/// passes, and an env plan's entry is always stamped — so an equality-only
/// lookup finds nothing there and every caller degrades silently (an empty
/// allowed-platform set composes no env at all).
fn plan_entry_for_version<'a>(plan: &'a PlanReport, version: &str) -> Option<&'a PlanVersionEntry> {
    plan.versions.iter().find(|entry| {
        entry.version == version || entry.source_version == version || is_build_stamp_of(version, &entry.source_version)
    })
}

/// Whether `tag` is `source_version` carrying a build-metadata stamp — i.e.
/// the tag `normalizer::env_version_tag` would have produced for it in some
/// earlier run of this pipeline.
///
/// Compared through `ocx_lib::Version` rather than by string prefix so the
/// build separator (`+` on the wire, `_` in a tag) is normalised on both
/// sides, the same way `spec::strip_build` decides platform applicability.
fn is_build_stamp_of(tag: &str, source_version: &str) -> bool {
    match (
        ocx_lib::package::version::Version::parse(tag),
        ocx_lib::package::version::Version::parse(source_version),
    ) {
        (Some(tagged), Some(source)) => tagged.has_build() && spec::strip_build(&tagged) == source,
        _ => false,
    }
}

/// Standalone-prepare (no `--plan`) resolution for a `pypi` source: finds the
/// upstream PyPI version whose (bare) tag equals `version` — the same lookup
/// `build_tasks_for_version` does for the archive/binary path, needed here
/// because a `pypi` source (unlike `pylock`) has no committed lock to read
/// the app version from directly.
async fn resolve_pypi_app_version(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    version: &str,
) -> Result<String, MirrorError> {
    let upstream_versions = list_upstream_versions(spec, spec_dir).await?;

    upstream_versions
        .iter()
        // `--version` may name the published (build-stamped) tag as well as
        // the bare release — same either-form contract as
        // `build_env_tasks_from_lock`, which this feeds.
        .find(|info| info.version == version || is_build_stamp_of(version, &info.version))
        .map(|info| info.version.clone())
        .ok_or_else(|| MirrorError::SpecInvalid(vec![format!("version '{version}' not found in pypi source")]))
}

/// Fetches the interpreter package's advertised `(leaf identifier, platform)`
/// candidates — the per-platform manifest digests behind its tag. One network
/// round-trip per prepare run; each leg's pin is then selected locally from
/// this set by [`select_interpreter_pin`].
///
/// Pinning a platform LEAF manifest instead of the tag's top-level digest is
/// load-bearing (ocx ≥ 0.5.3): a tag's image index is rewritten on every
/// platform push and its old digest garbage-collected, so an index-digest pin
/// is rejected by the publish gate at push time.
async fn fetch_interpreter_candidates(
    interpreter_package: &str,
    client: &ocx_lib::oci::Client,
) -> Result<Vec<(ocx_lib::oci::Identifier, ocx_lib::oci::Platform)>, MirrorError> {
    let identifier = ocx_lib::oci::Identifier::parse(interpreter_package).map_err(|e| {
        MirrorError::PylockError(format!(
            "invalid interpreter package reference '{interpreter_package}': {e}"
        ))
    })?;
    let index = ocx_lib::oci::index::Index::from_remote(ocx_lib::oci::index::OciIndex::new(
        ocx_lib::oci::index::OciIndexConfig { client: client.clone() },
    ));
    let candidates = index
        .fetch_candidates(&identifier, ocx_lib::oci::index::IndexOperation::Resolve)
        .await
        .map_err(|e| {
            MirrorError::TargetError(format!(
                "failed to resolve interpreter candidates for '{interpreter_package}': {e:#}"
            ))
        })?;
    match candidates {
        Some(children) if !children.is_empty() => Ok(children),
        _ => Err(MirrorError::TargetError(format!(
            "interpreter package '{interpreter_package}' not found in its registry"
        ))),
    }
}

/// Selects the interpreter's platform-leaf pin for one env leg and wraps it
/// as the composed package's `PRIVATE` dependency. Pure local selection over
/// [`fetch_interpreter_candidates`]' result, using the same
/// [`ocx_lib::oci::select_best`] relation `ocx package create` pins with (D1
/// parity) — the mirror and a hand-run `create` cannot disagree on which
/// leaf a leg depends on.
fn select_interpreter_pin(
    interpreter_package: &str,
    candidates: &[(ocx_lib::oci::Identifier, ocx_lib::oci::Platform)],
    platform: &ocx_lib::oci::Platform,
) -> Result<ocx_lib::package::metadata::dependency::Dependency, MirrorError> {
    let winner = match ocx_lib::oci::select_best(platform, candidates) {
        ocx_lib::oci::Selection::Found(identifier) => identifier,
        ocx_lib::oci::Selection::None => {
            let available: Vec<String> = candidates.iter().map(|(_, p)| p.to_string()).collect();
            return Err(MirrorError::PylockError(format!(
                "interpreter package '{interpreter_package}' has no entry compatible with platform '{platform}' (available: {})",
                available.join(", ")
            )));
        }
        ocx_lib::oci::Selection::Ambiguous(tied) => {
            let tied: Vec<String> = tied.iter().map(|id| id.to_string()).collect();
            return Err(MirrorError::PylockError(format!(
                "interpreter package '{interpreter_package}' has {} entries tied for platform '{platform}': {}",
                tied.len(),
                tied.join(", ")
            )));
        }
    };
    let pinned = ocx_lib::oci::PinnedIdentifier::try_from(winner)
        .map_err(|e| MirrorError::TargetError(format!("interpreter identifier not pinnable: {e}")))?;
    Ok(ocx_lib::package::metadata::dependency::Dependency {
        identifier: pinned,
        visibility: ocx_lib::package::metadata::visibility::Visibility::PRIVATE,
        name: None,
    })
}

/// Read and parse a `plan.json` document written by `pipeline plan`.
async fn read_plan(path: &std::path::Path) -> Result<PlanReport, MirrorError> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| MirrorError::PlanError(format!("failed to read plan file '{}': {e}", path.display())))?;
    serde_json::from_str(&content)
        .map_err(|e| MirrorError::PlanError(format!("failed to parse plan file '{}': {e}", path.display())))
}

/// Build `MirrorTask`s for `version` from the resolved assets a `pipeline plan`
/// run already crawled — no source query (issue #160: N prepare matrix legs
/// re-crawling the source exhausted the GitHub GraphQL points budget).
///
/// `version` is matched against the plan entry's variant-prefixed normalized
/// tag (the string the workflow matrix carries). Spec-owned task fields
/// (target, verify, cascade, metadata, asset_type) come from the local spec;
/// only the asset resolution is taken from the plan.
fn build_tasks_from_plan(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    plan: &PlanReport,
    version: &str,
) -> Result<Vec<MirrorTask>, MirrorError> {
    let entry = plan
        .versions
        .iter()
        .find(|e| e.version == version)
        .ok_or_else(|| MirrorError::PlanError(format!("version '{version}' not present in plan")))?;

    if entry.assets.is_empty() {
        return Err(MirrorError::PlanError(format!(
            "plan entry for '{version}' carries no resolved assets — regenerate plan.json \
             with an ocx-mirror that emits schema_version >= 2"
        )));
    }

    let effective_variants = spec.effective_variants();
    let variant = effective_variants
        .iter()
        .find(|v| v.name == entry.variant)
        .ok_or_else(|| {
            MirrorError::PlanError(format!(
                "variant '{}' from plan not declared in spec",
                entry.variant.as_deref().unwrap_or("<default>")
            ))
        })?;

    let mut tasks = Vec::new();
    for asset in &entry.assets {
        // Re-check applicability for consistency with the crawl path; plan
        // already drops non-applicable pairs, so this only matters for
        // hand-edited plan documents.
        if !spec.platform_applies(&entry.source_version, &asset.platform) {
            continue;
        }

        let platform = asset
            .platform
            .parse()
            .map_err(|e| MirrorError::PlanError(format!("invalid platform '{}' in plan: {e}", asset.platform)))?;

        let asset_type = variant
            .asset_type
            .as_ref()
            .map(|at| at.resolve(&asset.platform))
            .unwrap_or(spec::AssetType::Archive { strip_components: None });

        tasks.push(MirrorTask {
            version: entry.source_version.clone(),
            normalized_version: entry.version.clone(),
            platform,
            download_url: asset.url.clone(),
            asset_name: asset.asset_name.clone(),
            target: spec.target.clone(),
            metadata_config: variant.metadata.clone(),
            bin_scan: variant.bin_scan,
            libc_lint: variant.libc_lint,
            verify_config: spec.verify.clone(),
            cascade: spec.cascade.enabled,
            spec_dir: spec_dir.to_path_buf(),
            asset_type,
            variant: variant.name.as_ref().map(|name| VariantContext {
                name: name.clone(),
                is_default: variant.is_default,
            }),
        });
    }

    Ok(tasks)
}

/// Build `MirrorTask`s for a specific version string across all resolved platforms.
///
/// Lists upstream versions, finds the one matching `version`, applies asset patterns,
/// and returns one task per resolved platform. Returns an empty Vec if the version
/// is not found (no error; caller decides whether to treat this as an error).
async fn build_tasks_for_version(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    version: &str,
) -> Result<Vec<MirrorTask>, MirrorError> {
    let upstream_versions = list_upstream_versions(spec, spec_dir).await?;

    let build_ts = normalizer::build_timestamp(&spec.build_timestamp);
    let effective_variants = spec.effective_variants();
    let mut tasks = Vec::new();

    for variant in &effective_variants {
        let patterns = variant
            .assets
            .compiled()
            .map_err(|e| MirrorError::SpecInvalid(vec![e]))?;

        for version_info in &upstream_versions {
            // Normalize the upstream version to compare against the requested version.
            let normalized = match normalizer::normalize_version(&version_info.version, &build_ts) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Apply variant prefix to match the normalized tag format.
            let tagged = match &variant.name {
                Some(name) => format!("{name}-{normalized}"),
                None => normalized.clone(),
            };

            // Skip versions that don't match the requested version.
            // Accept either the raw upstream version or the normalized/tagged form.
            let matches = version_info.version == version || normalized == version || tagged == version;
            if !matches {
                continue;
            }

            match resolver::resolve_assets(&version_info.assets, &patterns) {
                AssetResolution::Resolved(platforms) => {
                    for platform_asset in &platforms {
                        let platform_str = platform_asset.platform.to_string();
                        // Skip pairs the platform does not apply to (out-of-window
                        // or excluded). `pipeline plan` already drops them from the
                        // matrix; this keeps `prepare` consistent if invoked
                        // directly for such a `(version, platform)`.
                        if !spec.platform_applies(&version_info.version, &platform_str) {
                            continue;
                        }
                        let asset_type = variant
                            .asset_type
                            .as_ref()
                            .map(|at| at.resolve(&platform_str))
                            .unwrap_or(spec::AssetType::Archive { strip_components: None });

                        tasks.push(MirrorTask {
                            version: version_info.version.clone(),
                            normalized_version: tagged.clone(),
                            platform: platform_asset.platform.clone(),
                            download_url: platform_asset.url.clone(),
                            asset_name: platform_asset.asset_name.clone(),
                            target: spec.target.clone(),
                            metadata_config: variant.metadata.clone(),
                            bin_scan: variant.bin_scan,
                            libc_lint: variant.libc_lint,
                            verify_config: spec.verify.clone(),
                            cascade: spec.cascade.enabled,
                            spec_dir: spec_dir.to_path_buf(),
                            asset_type,
                            variant: variant.name.as_ref().map(|name| VariantContext {
                                name: name.clone(),
                                is_default: variant.is_default,
                            }),
                        });
                    }
                }
                AssetResolution::Ambiguous(amb) => {
                    for a in &amb {
                        log::warn!(
                            "[{}] Ambiguous asset match for version {} on {}: {:?}",
                            spec.name,
                            version_info.version,
                            a.platform,
                            a.matched_assets
                        );
                    }
                }
            }
        }
    }

    Ok(tasks)
}

#[cfg(test)]
#[path = "prepare/tests.rs"]
mod tests;
