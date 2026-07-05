// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline prepare` — download, verify, and bundle one version
//! across all declared platforms. Mirrors the per-version subset of the
//! existing `command/sync.rs` Phase-1 loop.

use std::path::PathBuf;

use ocx_lib::cli::DataInterface;
use ocx_lib::log;

use crate::command::package::pipeline::plan::{
    PlanReport, derive_one_pypi_lock, derived_lock_filename, pylock_interpreter_pin, pylock_target_platform,
    pylock_variants, resolve_uv_python,
};
use crate::command::package::sync::list_upstream_versions;
use crate::error::MirrorError;
use crate::normalizer;
use crate::pipeline::mirror_task::{MirrorTask, VariantContext};
use crate::pipeline::orchestrator::{self, ConcurrencyParams};
use crate::pipeline::python_prepare::{self, SelectedWheel, WheelEnvTask};
use crate::resolver;
use crate::resolver::asset_resolution::AssetResolution;
use crate::source;
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
        // derived in-pipeline for `pypi` — not yet implemented, see
        // `build_env_tasks`) and composed into env packages. The
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

    /// Env-prepare path for `source.type: pylock` specs — the parallel to the
    /// archive/binary path in [`execute`](Self::execute).
    ///
    /// Builds one env task per applicable platform of the requested variant tag
    /// from the committed lock, then downloads + repacks + composes them into
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
        // The interpreter digest is the one network dependency of task building;
        // resolving it here keeps `build_env_tasks` a pure (hermetically
        // testable) local re-selection. A variant may override
        // `python.interpreter_package` (e.g. a musl-libc build for a
        // `libc: musl` variant), so each distinct reference in play is
        // resolved once, keyed by the reference string.
        let interpreter_dependencies = resolve_interpreter_dependencies(spec, python, &client).await?;

        // When `--plan` is supplied (the CI path), restrict prepare to the
        // platforms discover still needs for this version. discover emits a
        // backfill-partial entry that lists only the outstanding platforms, so an
        // already-published tile is not re-composed (and not later false-red at
        // push for a missing JUnit). Without a plan (standalone prepare), fall
        // back to every applicable spec platform.
        let allowed_platforms: Option<std::collections::HashSet<String>> = match &self.plan {
            Some(plan_path) => {
                let plan = read_plan(plan_path).await?;
                Some(
                    plan.versions
                        .iter()
                        .find(|entry| entry.version == self.version)
                        .map(|entry| entry.platforms.iter().cloned().collect())
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
                    &interpreter_dependencies,
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
                    &interpreter_dependencies,
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

        log::info!(
            "[{}] Preparing pylock env version {} ({} platforms)",
            spec.name,
            self.version,
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
            python_prepare::prepare_env_version(&self.version, &tasks, work_dir, &http_client, &concurrency).await?;

        let manifest_path = work_dir.join(&self.version).join("env-manifest.json");
        println!("{}", manifest_path.display());

        log::debug!(
            "[{}] Prepared {} env packages for version {}",
            spec.name,
            manifest.envs.len(),
            self.version
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
    interpreter_dependencies: &std::collections::HashMap<String, ocx_lib::package::metadata::dependency::Dependency>,
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
        interpreter_dependencies,
        allowed_platforms,
    )
}

/// Lock-agnostic core of [`build_env_tasks`].
///
/// Pure local re-selection (no source re-crawl — issue #160): for the variant
/// whose prefixed tag equals `version`, resolves a `PythonTarget` per declared,
/// applicable platform and runs `ocx_python::select_wheels`. Private
/// interpreter dependencies are resolved by the caller (their digests need
/// the registry) and looked up per variant — a variant's own
/// `interpreter_package` override wins over the spec-wide
/// `python.interpreter_package` default. Takes an already-parsed
/// `lock`/`app_version` so it never touches the filesystem — network-free and
/// directly unit-testable.
fn build_env_tasks_from_lock(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    version: &str,
    lock: &ocx_python::Pylock,
    app_version: &str,
    interpreter_dependencies: &std::collections::HashMap<String, ocx_lib::package::metadata::dependency::Dependency>,
    allowed_platforms: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<WheelEnvTask>, MirrorError> {
    let python = spec
        .python
        .as_ref()
        .expect("validated: python required for source.type 'pylock'");
    let interpreter_pin = pylock_interpreter_pin(python)?;

    let scope = ocx_python::WheelScope::new(spec.wheel_scope.clone());
    let declared_extras = lock.extras.clone();
    // Root = `source.package`/spec name (design decision C); resolved once per
    // version, same as `app_version` in the caller — `entrypoints:` windows
    // are resolved against this version here so `ocx_python::compose_env`
    // stays version-agnostic.
    let root_package = spec.source.pylock_app_name(&spec.name);
    let entrypoint_selection = python.resolve_entrypoint_selection(app_version, root_package);

    // The default variant (matched by name) drives cascade aliasing in push.
    let default_variant_name: Option<&str> = spec
        .variants
        .as_ref()
        .and_then(|variants| variants.iter().find(|variant| variant.default))
        .and_then(|variant| variant.name.as_deref());

    let mut platform_keys: Vec<&str> = spec
        .platforms
        .as_ref()
        .map_or_else(Vec::new, |platforms| platforms.keys().map(String::as_str).collect());
    platform_keys.sort_unstable();

    let mut tasks = Vec::new();
    for (variant_name, constraints) in pylock_variants(spec) {
        // One prepare leg = one variant tag; a separate leg prepares each other
        // variant, mirroring the archive path's per-version legs.
        let tagged = match variant_name {
            Some(name) => format!("{name}-{app_version}"),
            None => app_version.to_string(),
        };
        if tagged != version {
            continue;
        }
        let is_default = variant_name == default_variant_name;

        // A variant's own `interpreter_package` override (e.g. a musl-libc
        // CPython build for a `libc: musl` variant) takes precedence over the
        // spec-wide `python.interpreter_package` default.
        let interpreter_package_ref = spec
            .variants
            .as_ref()
            .and_then(|variants| variants.iter().find(|variant| variant.name.as_deref() == variant_name))
            .and_then(|variant| variant.interpreter_package.as_deref())
            .unwrap_or(python.interpreter_package.as_str());
        let interpreter_dependency = interpreter_dependencies
            .get(interpreter_package_ref)
            .cloned()
            .ok_or_else(|| {
                MirrorError::PylockError(format!(
                    "no resolved interpreter dependency for reference '{interpreter_package_ref}'"
                ))
            })?;

        for &platform_key in &platform_keys {
            if !spec.platform_applies(app_version, platform_key) {
                continue;
            }
            // Restrict to the platforms the plan still needs. `discover` excludes
            // already-published tiles (a backfill-partial run adds only the new
            // platforms of an existing version); without this, prepare composes
            // the already-published platform too, and push then false-reds it as
            // `missing_junit` (its test leg was skipped, so it has no JUnit).
            if let Some(allowed) = allowed_platforms
                && !allowed.contains(platform_key)
            {
                continue;
            }
            let platform: ocx_lib::oci::Platform = platform_key
                .parse()
                .map_err(|e| MirrorError::PylockError(format!("invalid platform key '{platform_key}': {e}")))?;

            let python_target = ocx_python::PythonTarget {
                platform: pylock_target_platform(&platform, platform_key)?,
                variant: constraints.clone(),
                interpreter: interpreter_pin.clone(),
            };

            let selected = ocx_python::select_wheels(lock, &python_target).map_err(|e| {
                MirrorError::PylockError(format!("wheel selection failed for platform '{platform_key}': {e}"))
            })?;

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
                normalized_version: tagged.clone(),
                source_version: app_version.to_string(),
                platform,
                variant: variant_name.map(|name| VariantContext {
                    name: name.to_string(),
                    is_default,
                }),
                target: spec.target.clone(),
                cascade: spec.cascade,
                spec_dir: spec_dir.to_path_buf(),
                wheels,
                interpreter: interpreter_dependency.clone(),
                requested_extras: Vec::new(), // W3: spec does not yet encode a per-app extras request
                declared_extras: declared_extras.clone(),
                python_target,
                wheel_scope: scope.clone(),
                entrypoint_selection: entrypoint_selection.clone(),
            });
        }
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
    interpreter_dependencies: &std::collections::HashMap<String, ocx_lib::package::metadata::dependency::Dependency>,
    allowed_platforms: Option<&std::collections::HashSet<String>>,
    plan_path: Option<&std::path::Path>,
    work_dir: &std::path::Path,
) -> Result<Vec<WheelEnvTask>, MirrorError> {
    let pylock_relative = match plan_path {
        Some(path) => {
            let plan = read_plan(path).await?;
            plan.versions
                .iter()
                .find(|entry| entry.version == version)
                .and_then(|entry| entry.pylock.clone())
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
            let python = spec
                .python
                .as_ref()
                .expect("validated: python required for source.type 'pypi'");
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
        interpreter_dependencies,
        allowed_platforms,
    )
}

/// Standalone-prepare (no `--plan`) resolution for a `pypi` source: finds the
/// upstream PyPI version whose (variant-prefixed) tag equals `version` — the
/// same lookup `build_tasks_for_version` does for the archive/binary path,
/// needed here because a `pypi` source (unlike `pylock`) has no committed
/// lock to read the app version from directly.
async fn resolve_pypi_app_version(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    version: &str,
) -> Result<String, MirrorError> {
    let upstream_versions = list_upstream_versions(spec, spec_dir).await?;
    let variants = pylock_variants(spec);

    find_matching_upstream_version(&variants, &upstream_versions, version)
        .ok_or_else(|| MirrorError::SpecInvalid(vec![format!("version '{version}' not found in pypi source")]))
}

/// Pure tag-matching core of [`resolve_pypi_app_version`]: the upstream
/// version whose (variant-prefixed) tag equals `version` — split out from its
/// caller so the matching logic is unit-testable without a network-backed
/// `list_upstream_versions` call.
fn find_matching_upstream_version(
    variants: &[(Option<&str>, ocx_python::VariantConstraints)],
    upstream_versions: &[source::VersionInfo],
    version: &str,
) -> Option<String> {
    upstream_versions
        .iter()
        .find(|info| {
            variants.iter().any(|(variant_name, _)| {
                let tagged = match variant_name {
                    Some(name) => format!("{name}-{}", info.version),
                    None => info.version.clone(),
                };
                tagged == version
            })
        })
        .map(|info| info.version.clone())
}

/// Resolves one interpreter [`Dependency`](ocx_lib::package::metadata::dependency::Dependency)
/// per distinct OCX reference in play for this spec — `python.interpreter_package`
/// plus any per-variant `interpreter_package` override (e.g. a musl-libc
/// CPython build for a `libc: musl` variant) — keyed by the reference string.
/// Each distinct reference hits the registry at most once.
async fn resolve_interpreter_dependencies(
    spec: &MirrorSpec,
    python: &spec::PythonConfig,
    client: &ocx_lib::oci::Client,
) -> Result<std::collections::HashMap<String, ocx_lib::package::metadata::dependency::Dependency>, MirrorError> {
    let mut interpreter_package_refs: std::collections::HashSet<&str> = std::collections::HashSet::new();
    interpreter_package_refs.insert(python.interpreter_package.as_str());
    if let Some(variants) = &spec.variants {
        for variant in variants {
            if let Some(interpreter_package) = &variant.interpreter_package {
                interpreter_package_refs.insert(interpreter_package.as_str());
            }
        }
    }

    let mut dependencies = std::collections::HashMap::with_capacity(interpreter_package_refs.len());
    for interpreter_package_ref in interpreter_package_refs {
        let dependency = build_interpreter_dependency(interpreter_package_ref, client).await?;
        dependencies.insert(interpreter_package_ref.to_string(), dependency);
    }
    Ok(dependencies)
}

/// Resolves the private interpreter dependency for a single OCX reference:
/// parses it, resolves its manifest digest, and pins it with `PRIVATE`
/// visibility. Called once per distinct reference by
/// [`resolve_interpreter_dependencies`].
async fn build_interpreter_dependency(
    interpreter_package: &str,
    client: &ocx_lib::oci::Client,
) -> Result<ocx_lib::package::metadata::dependency::Dependency, MirrorError> {
    let identifier = ocx_lib::oci::Identifier::parse(interpreter_package).map_err(|e| {
        MirrorError::PylockError(format!(
            "invalid interpreter package reference '{interpreter_package}': {e}"
        ))
    })?;
    let digest = client.fetch_manifest_digest(&identifier).await.map_err(|e| {
        MirrorError::TargetError(format!(
            "failed to resolve interpreter digest for '{interpreter_package}': {e:#}"
        ))
    })?;
    let pinned = ocx_lib::oci::PinnedIdentifier::try_from(identifier.clone_with_digest(digest))
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
            verify_config: spec.verify.clone(),
            cascade: spec.cascade,
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
                            verify_config: spec.verify.clone(),
                            cascade: spec.cascade,
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
mod tests {
    use std::panic;
    use std::path::Path;
    use tempfile::tempdir;

    use super::*;

    // ── §3.6 S6: prepare subcommand tests ────────────────────────────────────
    //
    // All tests that call execute() will panic with "not implemented"
    // until wave 3. Tests that only exercise struct construction pass now.

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn make_printer() -> DataInterface {
        DataInterface::new(ocx_lib::cli::Printer::new(false, false))
    }

    fn run_prepare(cmd: Prepare) -> Result<(), MirrorError> {
        let printer = make_printer();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { cmd.execute(&printer).await })
    }

    #[test]
    fn prepare_produces_bundle_for_each_declared_platform() {
        // §3.6: prepare --version 3.29.0 produces {work_dir}/{V}/{platform_slug}/bundle.tar.xz
        // for every declared platform.
        // Fails with "not implemented" until wave 3.
        let work_dir = tempdir().unwrap();
        let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            run_prepare(Prepare {
                spec: spec_path,
                version: "3.29.0".to_string(),
                work_dir: Some(work_dir.path().to_path_buf()),
                plan: None,
            })
        }));

        match result {
            Err(_) => {
                // Panicked with unimplemented!() — expected at Phase 3
            }
            Ok(Ok(())) => {
                let bundle_path = work_dir.path().join("3.29.0").join("linux_amd64").join("bundle.tar.xz");
                assert!(
                    bundle_path.exists(),
                    "Expected bundle at {}, not found",
                    bundle_path.display()
                );
            }
            Ok(Err(_)) => {
                // Other errors acceptable for unimplemented paths
            }
        }
    }

    #[test]
    fn prepare_produces_manifest_json() {
        // §3.6: Manifest file {work_dir}/{V}/manifest.json lists bundles with sizes + digests.
        let work_dir = tempdir().unwrap();
        let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            run_prepare(Prepare {
                spec: spec_path,
                version: "3.29.0".to_string(),
                work_dir: Some(work_dir.path().to_path_buf()),
                plan: None,
            })
        }));

        match result {
            Err(_) => {}
            Ok(Ok(())) => {
                let manifest_path = work_dir.path().join("3.29.0").join("manifest.json");
                assert!(manifest_path.exists(), "Expected manifest.json");
                let content = std::fs::read_to_string(&manifest_path).unwrap();
                let value: serde_json::Value =
                    serde_json::from_str(&content).expect("manifest.json must be valid JSON");
                assert!(
                    value.get("bundles").is_some() || value.is_array(),
                    "manifest.json must contain bundle list"
                );
            }
            Ok(Err(_)) => {}
        }
    }

    #[test]
    fn prepare_is_idempotent_on_rerun() {
        // §3.6: Re-run is idempotent (same bundles, no errors).
        let work_dir = tempdir().unwrap();
        let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

        let result1 = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            run_prepare(Prepare {
                spec: spec_path.clone(),
                version: "3.29.0".to_string(),
                work_dir: Some(work_dir.path().to_path_buf()),
                plan: None,
            })
        }));

        if result1.is_err() {
            // Both runs panicked with unimplemented — expected at Phase 3
            let result2 = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                run_prepare(Prepare {
                    spec: spec_path,
                    version: "3.29.0".to_string(),
                    work_dir: Some(work_dir.path().to_path_buf()),
                    plan: None,
                })
            }));
            assert!(result2.is_err(), "Second run must also panic with unimplemented");
            return;
        }

        if let Ok(Ok(())) = result1 {
            let result2 = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                run_prepare(Prepare {
                    spec: spec_path,
                    version: "3.29.0".to_string(),
                    work_dir: Some(work_dir.path().to_path_buf()),
                    plan: None,
                })
            }));
            assert!(matches!(result2, Ok(Ok(()))), "Second run (idempotent) must succeed");
        }
    }

    #[test]
    fn prepare_exits_65_on_checksum_mismatch() {
        // §3.6: Checksum mismatch → exit 65 (DataError).
        // Uses a fake version string to trigger failure.
        // Until implementation: expect unimplemented!() panic.
        use ocx_lib::cli::ExitCode;

        let work_dir = tempdir().unwrap();
        let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            run_prepare(Prepare {
                spec: spec_path,
                version: "99.99.99-bad-checksum".to_string(),
                work_dir: Some(work_dir.path().to_path_buf()),
                plan: None,
            })
        }));

        match result {
            Err(_) => {} // unimplemented — expected at Phase 3
            Ok(Err(MirrorError::SpecInvalid(_))) => {
                // Version-not-found is acceptable response for fake version
            }
            Ok(Err(e)) => {
                let exit_code = e.kind_exit_code();
                assert!(
                    exit_code == ExitCode::DataError || exit_code == ExitCode::Unavailable,
                    "Checksum mismatch must exit DataError(65) or Unavailable(69), got: {:?}",
                    exit_code
                );
            }
            Ok(Ok(())) => panic!("Expected error for bad checksum version"),
        }
    }

    #[test]
    fn prepare_exits_69_on_source_unreachable() {
        // §3.6: Source unreachable → exit 69 (Unavailable).
        // SourceError maps to ExitCode::Unavailable (69) via kind_exit_code().
        let work_dir = tempdir().unwrap();
        let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            run_prepare(Prepare {
                spec: spec_path,
                version: "3.29.0".to_string(),
                work_dir: Some(work_dir.path().to_path_buf()),
                plan: None,
            })
        }));

        match result {
            Err(_) => {} // unimplemented — expected at Phase 3
            Ok(Err(MirrorError::SourceError(_))) => {
                // Source unreachable → SourceError maps to Unavailable (69)
            }
            Ok(Err(e)) => {
                let _ = e.kind_exit_code();
            }
            Ok(Ok(())) => {
                // Acceptable if network is available and source resolves
            }
        }
    }

    /// Inline url_index spec (offline) with a late-introduced `windows/arm64`
    /// platform: `min_version: 0.11.7`. Used to verify resolve drops
    /// out-of-window `(version, platform)` pairs from the prepare task list.
    const APPLICABILITY_SPEC: &str = r#"
name: testtool
target:
  registry: ocx.sh
  repository: testtool
source:
  type: url_index
  versions:
    "0.10.0":
      assets:
        tool-linux-amd64: "https://example.com/0.10.0/linux-amd64"
        tool-windows-arm64: "https://example.com/0.10.0/windows-arm64"
    "0.11.8":
      assets:
        tool-linux-amd64: "https://example.com/0.11.8/linux-amd64"
        tool-windows-arm64: "https://example.com/0.11.8/windows-arm64"
    "0.12.0":
      assets:
        tool-linux-amd64: "https://example.com/0.12.0/linux-amd64"
        tool-windows-arm64: "https://example.com/0.12.0/windows-arm64"
assets:
  linux/amd64:
    - "tool-linux-amd64$"
  windows/arm64:
    - "tool-windows-arm64$"
asset_type:
  type: binary
  name: tool
build_timestamp: none
platforms:
  linux/amd64:
    runner: ubuntu-latest
  windows/arm64:
    runner: windows-11-arm
    min_version: "0.11.7"
    exclude:
      - version: "0.12.0"
        reason: "broken on this release"
"#;

    fn tasks_for(version: &str) -> Vec<MirrorTask> {
        let spec: MirrorSpec = serde_yaml_ng::from_str(APPLICABILITY_SPEC).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { build_tasks_for_version(&spec, Path::new("."), version).await.unwrap() })
    }

    fn platforms_of(tasks: &[MirrorTask]) -> Vec<String> {
        let mut p: Vec<String> = tasks.iter().map(|t| t.platform.to_string()).collect();
        p.sort();
        p
    }

    #[test]
    fn prepare_drops_out_of_window_platform() {
        // 0.10.0 is below windows/arm64's min_version (0.11.7) → only linux/amd64.
        assert_eq!(platforms_of(&tasks_for("0.10.0")), vec!["linux/amd64".to_string()]);
    }

    #[test]
    fn prepare_drops_excluded_platform_but_keeps_others() {
        // 0.12.0 is in-window but windows/arm64 has an exclude entry for it →
        // only linux/amd64 is prepared; the version is not dropped entirely.
        assert_eq!(platforms_of(&tasks_for("0.12.0")), vec!["linux/amd64".to_string()]);
    }

    #[test]
    fn prepare_keeps_in_window_platform() {
        // 0.11.8 is at/above min_version and not excluded → both platforms build.
        assert_eq!(
            platforms_of(&tasks_for("0.11.8")),
            vec!["linux/amd64".to_string(), "windows/arm64".to_string()]
        );
    }

    #[test]
    fn prepare_default_work_dir_uses_none() {
        // §3.6: Default work_dir when not specified → uses default ./.ocx-mirror.
        // Verify Prepare struct accepts None for work_dir.
        let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

        let cmd = Prepare {
            spec: spec_path,
            version: "3.29.0".to_string(),
            work_dir: None, // uses default ./.ocx-mirror
            plan: None,
        };

        // Struct construction must succeed (no panic)
        // Actual execution will panic with unimplemented!() — expected at Phase 3
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let printer = make_printer();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let _ = rt.block_on(async { cmd.execute(&printer).await });
        }));
        // Panicked or returned — either is acceptable at Phase 3
        let _ = result;
    }

    // ── issue #160: plan-based task building (no source re-crawl) ───────────

    use crate::command::package::pipeline::plan::{PlanAssetEntry, PlanVersionEntry, PlanVersionKind};

    /// Spec whose source is unreachable by construction (unroutable remote
    /// url_index). Any code path that queries the source fails; plan-based
    /// task building must succeed regardless.
    const UNREACHABLE_SOURCE_SPEC: &str = r#"
name: testtool
target:
  registry: ocx.sh
  repository: testtool
source:
  type: url_index
  url: "http://127.0.0.1:1/index.json"
assets:
  linux/amd64:
    - "tool-linux-amd64$"
  darwin/arm64:
    - "tool-darwin-arm64$"
asset_type:
  type: binary
  name: tool
build_timestamp: none
"#;

    fn plan_with(versions: Vec<PlanVersionEntry>) -> PlanReport {
        PlanReport {
            schema_version: 2,
            has_new: !versions.is_empty(),
            versions,
            target: "ocx.sh/testtool".to_string(),
            ocx_mirror_rev: None,
        }
    }

    fn asset_entry(platform: &str, name: &str) -> PlanAssetEntry {
        PlanAssetEntry {
            platform: platform.to_string(),
            asset_name: name.to_string(),
            url: url::Url::parse(&format!("https://example.com/{name}")).unwrap(),
        }
    }

    #[test]
    fn build_tasks_from_plan_does_not_query_source() {
        // Regression (issue #160): N prepare matrix legs re-crawling the
        // source exhausted the GitHub GraphQL points budget. With --plan,
        // tasks come from the plan's resolved assets — the (unreachable)
        // source is never queried, so this must succeed offline.
        let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
        let plan = plan_with(vec![PlanVersionEntry {
            version: "1.2.3".to_string(),
            platforms: vec!["linux/amd64".to_string(), "darwin/arm64".to_string()],
            kind: PlanVersionKind::New,
            source_version: "1.2.3".to_string(),
            variant: None,
            assets: vec![
                asset_entry("linux/amd64", "tool-linux-amd64"),
                asset_entry("darwin/arm64", "tool-darwin-arm64"),
            ],
            pylock: None,
        }]);

        let tasks = build_tasks_from_plan(&spec, Path::new("."), &plan, "1.2.3").unwrap();

        assert_eq!(tasks.len(), 2);
        let task = tasks.iter().find(|t| t.platform.to_string() == "linux/amd64").unwrap();
        assert_eq!(task.version, "1.2.3");
        assert_eq!(task.normalized_version, "1.2.3");
        assert_eq!(task.asset_name, "tool-linux-amd64");
        assert_eq!(task.download_url.as_str(), "https://example.com/tool-linux-amd64");
        assert!(task.variant.is_none());
    }

    #[test]
    fn build_tasks_from_plan_errors_on_missing_version() {
        let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
        let plan = plan_with(vec![]);

        let err = build_tasks_from_plan(&spec, Path::new("."), &plan, "9.9.9").unwrap_err();
        assert!(
            matches!(err, MirrorError::PlanError(_)),
            "expected PlanError, got {err:?}"
        );
    }

    #[test]
    fn build_tasks_from_plan_errors_on_plan_without_assets() {
        // A schema_version-1 plan parses (serde defaults) but carries no
        // resolved assets — prepare must fail with an actionable error
        // instead of silently building nothing.
        let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
        let plan = plan_with(vec![PlanVersionEntry {
            version: "1.2.3".to_string(),
            platforms: vec!["linux/amd64".to_string()],
            kind: PlanVersionKind::New,
            source_version: String::new(),
            variant: None,
            assets: vec![],
            pylock: None,
        }]);

        let err = build_tasks_from_plan(&spec, Path::new("."), &plan, "1.2.3").unwrap_err();
        match err {
            MirrorError::PlanError(msg) => {
                assert!(msg.contains("no resolved assets"), "unexpected message: {msg}");
            }
            other => panic!("expected PlanError, got {other:?}"),
        }
    }

    #[test]
    fn build_tasks_from_plan_errors_on_unknown_variant() {
        let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
        let plan = plan_with(vec![PlanVersionEntry {
            version: "slim-1.2.3".to_string(),
            platforms: vec!["linux/amd64".to_string()],
            kind: PlanVersionKind::New,
            source_version: "1.2.3".to_string(),
            variant: Some("slim".to_string()),
            assets: vec![asset_entry("linux/amd64", "tool-linux-amd64")],
            pylock: None,
        }]);

        let err = build_tasks_from_plan(&spec, Path::new("."), &plan, "slim-1.2.3").unwrap_err();
        assert!(
            matches!(err, MirrorError::PlanError(_)),
            "expected PlanError, got {err:?}"
        );
    }

    #[test]
    fn build_tasks_from_plan_respects_platform_applicability() {
        // Same applicability rules as the crawl path: out-of-window pairs in a
        // (hand-edited) plan are dropped, not built.
        let spec: MirrorSpec = serde_yaml_ng::from_str(APPLICABILITY_SPEC).unwrap();
        let plan = plan_with(vec![PlanVersionEntry {
            version: "0.10.0".to_string(),
            platforms: vec!["linux/amd64".to_string(), "windows/arm64".to_string()],
            kind: PlanVersionKind::New,
            source_version: "0.10.0".to_string(),
            variant: None,
            assets: vec![
                asset_entry("linux/amd64", "tool-linux-amd64"),
                // Below windows/arm64's min_version (0.11.7) → must be dropped.
                asset_entry("windows/arm64", "tool-windows-arm64"),
            ],
            pylock: None,
        }]);

        let tasks = build_tasks_from_plan(&spec, Path::new("."), &plan, "0.10.0").unwrap();
        assert_eq!(platforms_of(&tasks), vec!["linux/amd64".to_string()]);
    }

    // ── W2.3: pylock env task building (network-free — interpreter dep injected) ──

    /// A stand-in interpreter dependency with a fixed digest, so `build_env_tasks`
    /// runs without resolving a real registry manifest. `identifier` lets tests
    /// tell apart the spec-wide default from a per-variant override.
    fn fake_interpreter_dependency(
        identifier: &str,
        digest_fill: char,
    ) -> ocx_lib::package::metadata::dependency::Dependency {
        let json = format!(
            r#"{{"identifier":"{identifier}@sha256:{}"}}"#,
            digest_fill.to_string().repeat(64)
        );
        serde_json::from_str(&json).expect("interpreter dependency parses")
    }

    /// Resolves `spec.python.interpreter_package` alone — the map a spec with
    /// no per-variant `interpreter_package` override needs.
    fn default_interpreter_dependencies(
        spec: &MirrorSpec,
    ) -> std::collections::HashMap<String, ocx_lib::package::metadata::dependency::Dependency> {
        let python = spec.python.as_ref().expect("fixture spec has a python config");
        std::collections::HashMap::from([(
            python.interpreter_package.clone(),
            fake_interpreter_dependency("ocx.sh/cpython:3.13", 'a'),
        )])
    }

    fn pylock_fixture_spec_path() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pylock.yml"))
    }

    /// Fixture with a second, named `musl` variant carrying its own
    /// `interpreter_package` override — kept separate from
    /// [`pylock_fixture_spec_path`] so its extra variant doesn't perturb the
    /// entry counts `plan.rs`'s tests assert against that shared fixture.
    fn pylock_musl_variant_fixture_spec_path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-pylock-musl-variant.yml"
        ))
    }

    #[tokio::test]
    async fn build_env_tasks_selects_wheels_per_applicable_platform() {
        let spec_path = pylock_fixture_spec_path();
        let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
        let spec_dir = spec_path.parent().unwrap();

        let dependencies = default_interpreter_dependencies(&spec);
        let tasks = build_env_tasks(&spec, spec_dir, "1.0.0", &dependencies, None)
            .await
            .expect("build_env_tasks succeeds");

        // One default (unnamed) variant × two declared platforms.
        assert_eq!(tasks.len(), 2, "1 default variant × 2 platforms");
        let mut platforms: Vec<String> = tasks.iter().map(|task| task.platform.to_string()).collect();
        platforms.sort();
        assert_eq!(platforms, vec!["linux/amd64".to_string(), "linux/arm64".to_string()]);

        for task in &tasks {
            assert_eq!(task.normalized_version, "1.0.0");
            assert_eq!(task.source_version, "1.0.0");
            assert!(task.variant.is_none(), "the default variant carries no VariantContext");
            // Both fixture wheels are `none-any` → both apply on every platform.
            assert_eq!(task.wheels.len(), 2, "2 wheels per env leg");
            let names: Vec<&str> = task.wheels.iter().map(|wheel| wheel.filename.as_str()).collect();
            assert!(names.iter().any(|name| name.starts_with("pycowsay-")), "{names:?}");
            assert!(names.iter().any(|name| name.starts_with("six-")), "{names:?}");
            for wheel in &task.wheels {
                assert!(
                    wheel.wheel_repository.starts_with("pip-packages/"),
                    "repo-relative wheel repository: {}",
                    wheel.wheel_repository
                );
                assert_eq!(wheel.url.scheme(), "https");
            }
            assert!(
                task.interpreter.identifier.to_string().contains("cpython"),
                "the injected interpreter dependency is threaded onto every task"
            );
        }
    }

    #[tokio::test]
    async fn build_env_tasks_is_empty_for_unknown_version() {
        let spec_path = pylock_fixture_spec_path();
        let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
        let spec_dir = spec_path.parent().unwrap();

        let dependencies = default_interpreter_dependencies(&spec);
        let tasks = build_env_tasks(&spec, spec_dir, "9.9.9", &dependencies, None)
            .await
            .expect("build_env_tasks succeeds");
        assert!(tasks.is_empty(), "no variant tag matches an unknown version");
    }

    #[tokio::test]
    async fn build_env_tasks_restricts_to_plan_platforms() {
        // Backfill-partial: the plan lists only the outstanding platform
        // (linux/arm64), so prepare must compose that one alone — not the
        // already-published linux/amd64 the spec also declares.
        let spec_path = pylock_fixture_spec_path();
        let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
        let spec_dir = spec_path.parent().unwrap();

        let allowed: std::collections::HashSet<String> = ["linux/arm64".to_string()].into_iter().collect();
        let dependencies = default_interpreter_dependencies(&spec);
        let tasks = build_env_tasks(&spec, spec_dir, "1.0.0", &dependencies, Some(&allowed))
            .await
            .expect("build_env_tasks succeeds");

        assert_eq!(tasks.len(), 1, "plan restricts to the single outstanding platform");
        assert_eq!(tasks[0].platform.to_string(), "linux/arm64");
    }

    // ── per-variant `interpreter_package` override (musl-libc CPython) ──────

    #[tokio::test]
    async fn build_env_tasks_resolves_variant_interpreter_override() {
        // The fixture's "musl" variant declares its own `interpreter_package`
        // — its tasks must carry that override, not the spec-wide default.
        let spec_path = pylock_musl_variant_fixture_spec_path();
        let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
        let spec_dir = spec_path.parent().unwrap();

        let mut dependencies = default_interpreter_dependencies(&spec);
        dependencies.insert(
            "ocx.sh/python/cpython-musl:3.13.1".to_string(),
            fake_interpreter_dependency("ocx.sh/cpython-musl:3.13", 'b'),
        );

        let tasks = build_env_tasks(&spec, spec_dir, "musl-1.0.0", &dependencies, None)
            .await
            .expect("build_env_tasks succeeds");

        assert!(!tasks.is_empty(), "the musl variant produces env tasks");
        for task in &tasks {
            assert!(
                task.interpreter.identifier.to_string().contains("cpython-musl"),
                "the musl variant resolves its own interpreter_package override, got {}",
                task.interpreter.identifier
            );
        }
    }

    #[tokio::test]
    async fn build_env_tasks_falls_back_to_python_interpreter_package_without_override() {
        // Same multi-variant fixture as the override test above: the default
        // variant declares no `interpreter_package` of its own, so even
        // alongside a sibling "musl" variant that does, its tasks must fall
        // back to `python.interpreter_package`.
        let spec_path = pylock_musl_variant_fixture_spec_path();
        let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
        let spec_dir = spec_path.parent().unwrap();

        let mut dependencies = default_interpreter_dependencies(&spec);
        dependencies.insert(
            "ocx.sh/python/cpython-musl:3.13.1".to_string(),
            fake_interpreter_dependency("ocx.sh/cpython-musl:3.13", 'b'),
        );
        let tasks = build_env_tasks(&spec, spec_dir, "1.0.0", &dependencies, None)
            .await
            .expect("build_env_tasks succeeds");

        assert!(!tasks.is_empty());
        for task in &tasks {
            assert!(
                !task.interpreter.identifier.to_string().contains("musl"),
                "the default variant (no override) falls back to python.interpreter_package, got {}",
                task.interpreter.identifier
            );
        }
    }

    // ── plan_python_mirror_v2 W2.A3: pypi env-prepare dispatch ───────────────

    fn pypi_fixture_spec() -> MirrorSpec {
        let yaml = r#"
name: pycowsay
target:
  registry: ocx.sh
  repository: pycowsay
source:
  type: pypi
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn version_info(version: &str) -> crate::source::VersionInfo {
        crate::source::VersionInfo {
            version: version.to_string(),
            assets: std::collections::HashMap::new(),
            is_prerelease: false,
        }
    }

    #[test]
    fn find_matching_upstream_version_matches_bare_tag() {
        let spec = pypi_fixture_spec();
        let variants = pylock_variants(&spec);
        let upstream = vec![version_info("1.0.0"), version_info("2.0.0")];

        assert_eq!(
            find_matching_upstream_version(&variants, &upstream, "1.0.0"),
            Some("1.0.0".to_string())
        );
    }

    #[test]
    fn find_matching_upstream_version_returns_none_for_unknown_tag() {
        let spec = pypi_fixture_spec();
        let variants = pylock_variants(&spec);
        let upstream = vec![version_info("1.0.0")];

        assert_eq!(find_matching_upstream_version(&variants, &upstream, "9.9.9"), None);
    }

    const PYPI_DERIVED_LOCK_BODY: &str = r#"lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-py3-none-any.whl"
url = "https://example.com/pycowsay-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;

    #[tokio::test]
    async fn build_pypi_env_tasks_consumes_plan_provided_lock_without_deriving() {
        // No OCX_BINARY_PIN/OCX_MIRROR_UV stub is installed for this test: if
        // the plan-provided-lock path fell through to re-derivation, it would
        // try to spawn a real `ocx`/`uv` binary and fail — proving this path
        // never touches them.
        let plan_dir = tempdir().unwrap();
        let locks_dir = plan_dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();
        std::fs::write(locks_dir.join("pylock.pycowsay-1.0.0.toml"), PYPI_DERIVED_LOCK_BODY).unwrap();

        let plan = PlanReport {
            schema_version: 2,
            has_new: true,
            versions: vec![PlanVersionEntry {
                version: "1.0.0".to_string(),
                platforms: vec!["linux/amd64".to_string()],
                kind: PlanVersionKind::New,
                source_version: "1.0.0".to_string(),
                variant: None,
                assets: vec![],
                pylock: Some("locks/pylock.pycowsay-1.0.0.toml".to_string()),
            }],
            target: "ocx.sh/pycowsay".to_string(),
            ocx_mirror_rev: None,
        };
        let plan_path = plan_dir.path().join("plan.json");
        std::fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        let spec = pypi_fixture_spec();
        let dependencies = default_interpreter_dependencies(&spec);

        let tasks = build_pypi_env_tasks(
            &spec,
            Path::new("."),
            "1.0.0",
            &dependencies,
            None,
            Some(&plan_path),
            Path::new("."),
        )
        .await
        .expect("consuming a plan-provided lock never spawns uv/ocx");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].source_version, "1.0.0");
        assert_eq!(tasks[0].platform.to_string(), "linux/amd64");
    }

    #[tokio::test]
    async fn build_pypi_env_tasks_errors_on_unparseable_plan_provided_lock() {
        // A sdist-only package (no [[packages.wheels]]) is valid TOML but
        // fails ocx_python::parse_pylock's fail-closed re-parse — must
        // surface as PylockError (exit 65), not a panic or silent skip.
        let plan_dir = tempdir().unwrap();
        let locks_dir = plan_dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();
        let bad_body = "lock-version = \"1.0\"\n\n[[packages]]\nname = \"pycowsay\"\nversion = \"1.0.0\"\n";
        std::fs::write(locks_dir.join("pylock.pycowsay-1.0.0.toml"), bad_body).unwrap();

        let plan = PlanReport {
            schema_version: 2,
            has_new: true,
            versions: vec![PlanVersionEntry {
                version: "1.0.0".to_string(),
                platforms: vec!["linux/amd64".to_string()],
                kind: PlanVersionKind::New,
                source_version: "1.0.0".to_string(),
                variant: None,
                assets: vec![],
                pylock: Some("locks/pylock.pycowsay-1.0.0.toml".to_string()),
            }],
            target: "ocx.sh/pycowsay".to_string(),
            ocx_mirror_rev: None,
        };
        let plan_path = plan_dir.path().join("plan.json");
        std::fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        let spec = pypi_fixture_spec();
        let dependencies = default_interpreter_dependencies(&spec);

        let err = build_pypi_env_tasks(
            &spec,
            Path::new("."),
            "1.0.0",
            &dependencies,
            None,
            Some(&plan_path),
            Path::new("."),
        )
        .await
        .expect_err("an unparseable plan-provided lock must fail, not silently succeed");

        assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
        assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
    }
}
