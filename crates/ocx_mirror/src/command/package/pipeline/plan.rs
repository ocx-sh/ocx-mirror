// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline plan` — compute which versions need work without
//! side-effects. Used by the GHA `discover` job.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;
use ocx_lib::cli::DataInterface;
use ocx_lib::oci::{Architecture, ClientBuilder, OperatingSystem, Platform};
use ocx_lib::package::version::Version;
use ocx_lib::publisher::Publisher;
use ocx_python::{
    Implementation, InterpreterPin, LibcFamily, Pylock, PythonTarget, TargetArchitecture, TargetOperatingSystem,
    TargetPlatform, VariantConstraints,
};
use serde::{Deserialize, Serialize};

use crate::command::package::options::OutputFormat;
use crate::command::package::sync::list_upstream_versions;
use crate::command::package::target_registry;
use crate::error::MirrorError;
use crate::filter;
use crate::normalizer;
use crate::pipeline::lock_derive;
use crate::resolver;
use crate::resolver::asset_resolution::AssetResolution;
use crate::source;
use crate::spec::{self, BackfillOrder, LockOptions, MirrorSpec, PythonConfig, Source, WheelPatterns};
use crate::version_platform_map::VersionPlatformMap;

/// Default `--locks-dir` for `pipeline plan` — where derived PEP 751 locks
/// for `source.type: pypi` mirrors are written, relative to the command's
/// working directory (the same directory `plan.json` is written to via
/// stdout redirect in the generated workflow). Shared with `describe.rs`'s
/// catalog autogen, which looks for an already-derived lock in the same
/// place.
pub(crate) const DEFAULT_LOCKS_DIR: &str = "locks";

/// `new` | `backfill-partial` — what kind of work is needed for this version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanVersionKind {
    /// Version not yet present in the target registry.
    New,
    /// Version present for some platforms but missing for others.
    BackfillPartial,
}

/// A resolved per-platform asset carried in the plan so `prepare` legs can
/// build tasks without re-crawling the source (issue #160 — one crawl per
/// pipeline run instead of N+1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanAssetEntry {
    /// Platform slug (e.g. `linux/amd64`).
    pub platform: String,
    /// Upstream asset file name (drives archive-type detection downstream).
    pub asset_name: String,
    /// Direct download URL resolved by discover's single source crawl.
    pub url: url::Url,
}

/// A single version entry in the plan output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanVersionEntry {
    /// Normalized tag the pipeline publishes. Archive sources may carry a
    /// variant prefix (`slim-3.29.0`); env sources always emit the bare app
    /// version (libc is a platform `os.features` axis there, never a tag
    /// prefix). The whole prepare → test → push chain keys off this string.
    pub version: String,
    /// Base `os/arch` platform strings that require work (e.g.
    /// `["linux/amd64", "darwin/arm64"]`) — matches the CI matrix legs. Env
    /// entries dedupe `+libc.*` wheels keys onto their base here; the full
    /// keys live in [`assets`](Self::assets).
    pub platforms: Vec<String>,
    /// Kind of work needed.
    pub kind: PlanVersionKind,
    /// Raw upstream version string (pre-normalization, e.g. `3.29.0` for tag
    /// `3.29.0_20260610`). `prepare --plan` needs it for platform
    /// applicability checks and task construction.
    ///
    /// `#[serde(default)]` keeps schema_version-1 plans parseable; consumers
    /// requiring resolved data must check [`PlanVersionEntry::assets`] first.
    #[serde(default)]
    pub source_version: String,
    /// Variant name this entry belongs to (`None` = default variant).
    #[serde(default)]
    pub variant: Option<String>,
    /// Resolved assets for exactly the platforms in `platforms`. Carried so
    /// `prepare --plan` never re-runs the source generator (issue #160).
    #[serde(default)]
    pub assets: Vec<PlanAssetEntry>,
    /// Relative path (from plan.json's directory) of the derived pylock this entry
    /// was resolved from. Set only for `source.type: pypi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pylock: Option<String>,
}

/// Structured output of `ocx-mirror package pipeline plan`.
///
/// JSON shape (schema_version 2 — v2 adds `source_version`, `variant`, and
/// resolved `assets` per version entry so `prepare --plan` consumes the
/// discover crawl instead of re-crawling, issue #160):
/// ```json
/// {
///   "schema_version": 2,
///   "has_new": true,
///   "versions": [...],
///   "target": "ocx.sh/cmake",
///   "ocx_mirror_rev": "abc123..."
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanReport {
    /// Schema version for forward-compat detection.
    pub schema_version: u32,
    /// `true` when at least one version requires action.
    pub has_new: bool,
    /// Versions requiring action, oldest first.
    pub versions: Vec<PlanVersionEntry>,
    /// Full OCI repository identifier (registry/repo).
    pub target: String,
    /// The git SHA of `ocx-mirror` used when generating this plan.
    pub ocx_mirror_rev: Option<String>,
}

/// `ocx-mirror package pipeline plan` subcommand.
///
/// Reads `mirror.yml`, queries source + target registry, and emits a
/// side-effect-free plan document listing versions that need action.
#[derive(clap::Parser)]
pub struct PlanCmd {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Output format.
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Directory derived PEP 751 locks are written to (`source.type: pypi`
    /// only). Each pypi `PlanVersionEntry.pylock` carries a path relative to
    /// this directory's parent — i.e. relative to this command's working
    /// directory, same as `plan.json` itself. Unused for any other source
    /// type. Default: `./locks`.
    #[arg(long)]
    pub locks_dir: Option<PathBuf>,
}

impl PlanCmd {
    pub async fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        let spec_path = &self.spec;
        let spec = spec::load_spec(spec_path)
            .await
            .map_err(|e| MirrorError::SourceError(format!("failed to load spec: {e}")))?;
        let spec_dir = spec_path.parent().unwrap_or(std::path::Path::new("."));
        let locks_dir = self
            .locks_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_LOCKS_DIR));

        let report = build_plan_report(&spec, spec_dir, &locks_dir).await?;

        // Determine output format: explicit flag, or JSON when in GitHub Actions.
        let use_json = match self.format {
            Some(OutputFormat::Json) => true,
            Some(OutputFormat::Plain) => false,
            None => std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true"),
        };

        if use_json {
            printer
                .print_json(&report)
                .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to serialize plan: {e}")]))?;
        } else {
            print_plan_plain(&report);
        }

        Ok(())
    }
}

/// Core plan computation: load registry state, fetch upstream, filter, classify.
///
/// Extracted so that integration tests can call it without going through the
/// full CLI surface (file-system spec path, `Printer`, format detection).
async fn build_plan_report(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    locks_dir: &Path,
) -> Result<PlanReport, MirrorError> {
    // Build target identifier for registry queries.
    let client = ClientBuilder::from_env().map_err(|e| MirrorError::ExecutionFailed(vec![e.to_string()]))?;
    let publisher = Publisher::new(client);
    let identifier = ocx_lib::oci::Identifier::new_registry(&spec.target.repository, &spec.target.registry);

    // Fetch existing tags from the target registry to build the platform map.
    // Fail-safe (issue #157): only an authoritative "repository not found"
    // (first publish) yields an empty list; any other failure aborts the plan
    // so published versions are never re-flagged as new.
    let all_tags: Vec<String> = target_registry::list_target_tags(&publisher, &identifier).await?;

    // Determine which (version, platform) pairs are already present.
    let source_version_tags: HashSet<String> = {
        // Collect version-string forms we care about (including variant-prefixed).
        let mut tags = HashSet::new();
        for tag in &all_tags {
            if let Some(v) = Version::parse(tag) {
                tags.insert(v.to_string());
            }
        }
        tags
    };

    let tags_needing_platform_check: Vec<&str> = all_tags
        .iter()
        .filter(|t| source_version_tags.contains(t.as_str()))
        .map(String::as_str)
        .collect();

    // Fail-safe (issue #157): a transient manifest fetch failure aborts
    // instead of leaving the version's platform set empty (which would
    // classify it as New with the full platform set → republish).
    let platform_info =
        target_registry::fetch_published_platforms(&publisher, &identifier, &tags_needing_platform_check).await?;

    let version_map = VersionPlatformMap::from_tags_and_platforms(&all_tags, platform_info);

    // Fetch upstream versions. `list_upstream_versions` already classifies
    // the failure per source type (pylock: PylockError for malformed lock
    // content vs SourceError for an unreachable file; github_release/
    // url_index: always SourceError) — propagate as-is instead of
    // re-stamping every failure as SourceError, which would collapse a data
    // error into an availability one.
    let upstream_versions = list_upstream_versions(spec, spec_dir).await?;

    // Build timestamp (reuse existing normalizer).
    let build_ts = normalizer::build_timestamp(&spec.build_timestamp);

    // `pylock` selects wheel SETS (N per platform) via `ocx_python::select_wheels`
    // instead of the regex `resolve_assets`, which assumes exactly one asset per
    // platform and errors (`AmbiguousAsset`) on 2+ — structurally incompatible
    // with wheel sets (D1, plan_pylock_mirror.md). The branch builds its own
    // `PlanVersionEntry` list directly rather than joining the regex path below.
    match &spec.source {
        Source::Pylock { path, .. } => {
            let versions =
                build_pylock_plan_entries(spec, spec_dir, path, &upstream_versions, &all_tags, &version_map).await?;
            let target = format!("{}/{}", spec.target.registry, spec.target.repository);
            let ocx_mirror_rev = spec.ocx_mirror.as_ref().and_then(|c| c.rev.clone());
            return Ok(PlanReport {
                schema_version: 2,
                has_new: !versions.is_empty(),
                versions,
                target,
                ocx_mirror_rev,
            });
        }
        // Discovery already ran above via `list_upstream_versions` (dispatches
        // to `source::pypi::list_versions`); per-version lock derivation
        // happens inside `build_pypi_plan_entries` (design decision A,
        // plan_python_mirror_v2 W2.A3) — reuses the same lock-agnostic
        // `build_env_plan_entries` the `pylock` branch above calls, once a
        // lock has been derived for a candidate version.
        Source::Pypi { .. } => {
            let versions =
                build_pypi_plan_entries(spec, &upstream_versions, &all_tags, &version_map, locks_dir).await?;
            let target = format!("{}/{}", spec.target.registry, spec.target.repository);
            let ocx_mirror_rev = spec.ocx_mirror.as_ref().and_then(|c| c.rev.clone());
            return Ok(PlanReport {
                schema_version: 2,
                has_new: !versions.is_empty(),
                versions,
                target,
                ocx_mirror_rev,
            });
        }
        _ => {}
    }

    // Resolve assets per effective variant — same logic as sync.rs.
    let effective_variants = spec.effective_variants();
    let mut resolved_versions = Vec::new();

    for variant in &effective_variants {
        let patterns = variant
            .assets
            .compiled()
            .map_err(|e| MirrorError::SpecInvalid(vec![e]))?;

        for version_info in &upstream_versions {
            if let AssetResolution::Resolved(platforms) = resolver::resolve_assets(&version_info.assets, &patterns)
                && let Ok(normalized) = normalizer::normalize_version(&version_info.version, &build_ts)
            {
                // Drop `(version, platform)` pairs the platform does not apply to
                // (out-of-window or excluded per `platforms.<p>` applicability).
                // These then never reach plan.json, so discover never reports
                // them as "missing" and the pair is never scheduled/built/tested.
                let platforms: Vec<_> = platforms
                    .into_iter()
                    .filter(|asset| spec.platform_applies(&version_info.version, &asset.platform.to_string()))
                    .collect();

                let tagged = match &variant.name {
                    Some(name) => format!("{name}-{normalized}"),
                    None => normalized,
                };
                resolved_versions.push(filter::ResolvedVersion {
                    version: version_info.version.clone(),
                    normalized_version: tagged,
                    variant: variant.name.clone(),
                    platforms,
                    is_prerelease: version_info.is_prerelease,
                });
            }
        }
    }

    // Apply filter pipeline — no exact-version or latest flags for the plan command.
    let filtered = filter::filter_versions(
        resolved_versions,
        &[], // no exact-version pin
        spec.skip_prereleases,
        spec.versions.as_ref(),
        &version_map,
        false, // latest
    );

    // Classify each filtered version: New or BackfillPartial.
    //
    // After filter_versions, each ResolvedVersion.platforms contains ONLY the
    // platforms that still need work (filter_versions trims already-present tiles).
    // To distinguish New from BackfillPartial we need to know whether the version
    // has ANY tile already on the registry.
    //
    // Declared platform set comes from spec.platforms; if absent, every resolved
    // platform is "all declared" so any filtered version must be New.
    let declared_platform_count = spec.platforms.as_ref().map_or(0, |p| p.len());
    let version_entries = build_version_entries(&filtered, &all_tags, declared_platform_count);

    // Output is oldest-first (filter_versions already sorts semver ascending).
    let has_new = !version_entries.is_empty();

    let target = format!("{}/{}", spec.target.registry, spec.target.repository);
    let ocx_mirror_rev = spec.ocx_mirror.as_ref().and_then(|c| c.rev.clone());

    Ok(PlanReport {
        schema_version: 2,
        has_new,
        versions: version_entries,
        target,
        ocx_mirror_rev,
    })
}

/// Builds the `PlanVersionEntry` list for a `pylock`-sourced spec.
///
/// Thin wrapper: resolves the app version from the source adapter's
/// already-listed `VersionInfo`, loads the committed lock, and delegates the
/// actual per-platform wheel selection to the lock-agnostic
/// [`build_env_plan_entries`].
async fn build_pylock_plan_entries(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    path: &str,
    upstream_versions: &[source::VersionInfo],
    all_tags: &[String],
    version_map: &VersionPlatformMap,
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let app_version = upstream_versions
        .first()
        .map(|info| info.version.clone())
        .ok_or_else(|| MirrorError::PylockError("pylock source produced no version".to_string()))?;

    // The source adapter (list_upstream_versions, above) already parsed the
    // lock once to extract the app version; parsing it again here is the
    // price of keeping `source::VersionInfo` source-agnostic (no `Pylock`
    // leaking into it) — a committed local pylock.toml is small, so the extra
    // parse is cheaper than threading the parsed value across the source
    // boundary.
    let lock = source::pylock::load(spec_dir, path)
        .await
        .map_err(|e| source::pylock::classify_error("failed to load pylock source", e))?;

    build_env_plan_entries(spec, &lock, &app_version, all_tags, version_map)
}

/// Lock-agnostic core of [`build_pylock_plan_entries`].
///
/// Bypasses `resolve_assets`/`filter::filter_versions` entirely (D1): for
/// each declared `wheels:` platform key whose BASE os/arch
/// `spec.platform_applies` accepts and whose FULL key (os_features included)
/// is not already published (per `version_map`), resolves a `PythonTarget`
/// from the key + its effective filter and calls `ocx_python::select_wheels`
/// directly, emitting one `PlanAssetEntry` per selected wheel carrying the
/// full key. `platforms` dedupes onto base strings so the CI matrix gate
/// keeps matching `matrix.platform`. Takes an already-parsed
/// `lock`/`app_version` so it never touches the filesystem — network-free and
/// directly unit-testable.
fn build_env_plan_entries(
    spec: &MirrorSpec,
    lock: &Pylock,
    app_version: &str,
    all_tags: &[String],
    version_map: &VersionPlatformMap,
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let python = spec
        .python
        .as_ref()
        .expect("validated: python required for source.type 'pylock'");
    let interpreter = pylock_interpreter_pin(python)?;
    let wheels_map = spec
        .wheels
        .as_ref()
        .expect("validated: wheels required for env sources");

    let declared_platform_count = spec.platforms.as_ref().map_or(0, |platforms| platforms.len());

    // The pylock app version is a PEP 440 string, which may carry more
    // numeric components than `ocx_lib::Version` (a ≤3-component
    // tool-release-tag semver parser) accepts — pycowsay's `0.0.0.2`, or a
    // calendar version like `2024.1.1.1`. A tag that does not parse simply
    // cannot be present in the `Version`-keyed `version_map`, so it is
    // treated as outstanding work rather than panicking.
    //
    // ponytail: per-platform dedup of such non-semver versions is therefore
    // a no-op — a re-run re-publishes the (identical, content-addressed)
    // env, which the registry dedups. Precise PEP 440 dedup would need a
    // PEP 440-aware `version_map`; deferred (not blocking — publishes are
    // idempotent).
    let check_version = Version::parse(app_version);

    let mut missing_platforms: Vec<String> = Vec::new();
    let mut assets = Vec::new();

    for platform in wheels_map.sorted_platforms() {
        let key = platform.to_string();
        let base = spec::base_platform_key(platform);
        if !spec.platform_applies(app_version, &base) {
            continue;
        }
        if check_version
            .as_ref()
            .is_some_and(|version| version_map.has(version, platform))
        {
            continue; // already published for this full key (os_features included)
        }

        let target = PythonTarget {
            platform: pylock_target_platform(platform, &key)?,
            variant: wheel_target_constraints(wheels_map, platform),
            interpreter: interpreter.clone(),
        };

        let wheels = ocx_python::select_wheels(lock, &target)
            .map_err(|e| MirrorError::PylockError(format!("wheel selection failed for platform '{key}': {e}")))?;

        if !missing_platforms.contains(&base) {
            missing_platforms.push(base.clone());
        }
        for wheel in wheels {
            let url_str = wheel.url.ok_or_else(|| {
                MirrorError::PylockError(format!(
                    "wheel '{}' for package '{}' selected with no download URL",
                    wheel.filename, wheel.name
                ))
            })?;
            let url = url::Url::parse(&url_str)
                .map_err(|e| MirrorError::PylockError(format!("invalid wheel URL '{url_str}': {e}")))?;
            assets.push(PlanAssetEntry {
                platform: key.clone(),
                asset_name: wheel.filename,
                url,
            });
        }
    }

    if missing_platforms.is_empty() {
        return Ok(Vec::new());
    }

    // Same New/BackfillPartial convention as build_version_entries: the bare
    // (un-timestamped) tag already on the registry means some platform was
    // published before, so a shorter missing-set than the declared count is a
    // backfill, not a first publish.
    let version_on_registry = Version::parse(app_version)
        .is_some_and(|v| all_tags.iter().any(|t| Version::parse(t).is_some_and(|tv| tv == v)));
    let kind = if version_on_registry && declared_platform_count > missing_platforms.len() {
        PlanVersionKind::BackfillPartial
    } else {
        PlanVersionKind::New
    };

    Ok(vec![PlanVersionEntry {
        version: app_version.to_string(),
        platforms: missing_platforms,
        kind,
        source_version: app_version.to_string(),
        variant: None,
        assets,
        pylock: None,
    }])
}

/// Cheap pre-filter for `source.type: pypi` lock-derivation candidates:
/// `versions:` bounds, `skip_prereleases`, an already-published dedup check
/// (at least one declared `wheels:` key still outstanding), and
/// `new_per_run`/`backfill` — all applied BEFORE any `uv`/`ocx` subprocess
/// spawns, so [`build_pypi_plan_entries`] only pays the derivation cost
/// (interpreter materialization + `uv pip compile`) for versions that
/// actually have outstanding work.
///
/// Deliberately does not reuse `filter::filter_versions`: its already-
/// published dedup step `.expect()`s every tag to parse as `ocx_lib::Version`,
/// which panics on real PyPI version strings that string has more components
/// than that ≤3-component parser accepts (e.g. `0.0.0.2`) or a PEP 440
/// `uv`-only suffix (`2.0.0.dev0`) — the same reason `build_env_plan_entries`
/// bypasses it for `pylock` (D1, `plan_python_mirror_v2`). This mirrors that
/// function's fail-open convention instead: an unparseable tag is always
/// kept as outstanding work.
fn select_pypi_candidates<'a>(
    spec: &MirrorSpec,
    upstream_versions: &'a [source::VersionInfo],
    version_map: &VersionPlatformMap,
) -> Vec<&'a source::VersionInfo> {
    let wheels_keys: Vec<&Platform> = spec
        .wheels
        .as_ref()
        .map_or_else(Vec::new, WheelPatterns::sorted_platforms);

    let versions_config = spec.versions.as_ref();
    let min = versions_config
        .and_then(|c| c.min.as_ref())
        .and_then(|s| Version::parse(s));
    let max = versions_config
        .and_then(|c| c.max.as_ref())
        .and_then(|s| Version::parse(s));

    let mut candidates: Vec<&source::VersionInfo> = upstream_versions
        .iter()
        .filter(|info| !(spec.skip_prereleases && info.is_prerelease))
        .filter(|info| {
            let Some(parsed) = Version::parse(&info.version) else {
                return true; // keep unparseable versions (filter.rs convention)
            };
            !(min.as_ref().is_some_and(|m| parsed < *m) || max.as_ref().is_some_and(|m| parsed >= *m))
        })
        .filter(|info| {
            let tag_version = Version::parse(&info.version);
            wheels_keys.iter().any(|&platform| {
                spec.platform_applies(&info.version, &spec::base_platform_key(platform))
                    && match &tag_version {
                        Some(v) => !version_map.has(v, platform),
                        // Unparseable tag: cannot be in the Version-keyed
                        // map, so treat as outstanding.
                        None => true,
                    }
            })
        })
        .collect();

    candidates.sort_by(|a, b| match (Version::parse(&a.version), Version::parse(&b.version)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => a.version.cmp(&b.version),
    });

    if let Some(config) = versions_config
        && let Some(cap) = config.new_per_run
    {
        match config.backfill {
            BackfillOrder::OldestFirst => candidates.truncate(cap),
            BackfillOrder::NewestFirst => {
                let start = candidates.len().saturating_sub(cap);
                candidates = candidates.split_off(start);
            }
        }
    }

    candidates
}

/// Maps a [`lock_derive`] `String` error to the mirror's error taxonomy
/// (plan_python_mirror_v2 W3 acceptance contract: uv-fail→65, uv-missing→1).
///
/// Data errors — this version cannot produce a trustworthy lock — map to
/// [`MirrorError::PylockError`] (exit 65, same class as `select_wheels`
/// failures): `uv`'s nonzero exit (unsolvable requirements, bad package
/// metadata; the message carries uv's stderr tail) and `derive_pylock`'s
/// fail-closed re-parse rejection. Everything else — `uv` binary
/// missing/spawn failure, timeout, interpreter materialization, lock-file
/// I/O — is a subprocess execution failure ([`MirrorError::ExecutionFailed`],
/// exit 1), the same convention `describe.rs::invoke_describe` uses for
/// `ocx package describe` subprocess failures.
///
/// ponytail: string-sniffs the two data-error markers rather than a
/// structured `lock_derive::Error` enum — `lock_derive.rs` is out of scope
/// for this wiring task (dead-code removal only); promote to a real error
/// type if another call site needs to distinguish more sub-failures.
fn classify_lock_derive_error(err: String) -> MirrorError {
    if err.contains("failed to re-parse") || err.contains("uv pip compile exited") {
        MirrorError::PylockError(err)
    } else {
        MirrorError::ExecutionFailed(vec![err])
    }
}

/// The on-disk filename for a derived PEP 751 lock. `uv pip compile` REJECTS
/// output filenames that do not start with `pylock.` and end with `.toml`
/// (its own example shape: `pylock.dev.toml`) — found by the live W4 pypi
/// pilot; shared by the plan-phase candidate loop and `prepare.rs`'s
/// standalone re-derivation so the two sites cannot drift.
pub(crate) fn derived_lock_filename(package: &str, version: &str) -> String {
    format!("pylock.{package}-{version}.toml")
}

/// `python.lock`'s defaults, applied when a `pypi` spec omits the `lock:`
/// block entirely (zero-config: universal lock, no excludes, 300s timeout).
fn default_lock_options() -> LockOptions {
    LockOptions {
        universal: true,
        extras: Vec::new(),
        exclude: Vec::new(),
        timeout_seconds: 300,
    }
}

/// Resolves the [`lock_derive::UvPython`] selector for this spec's lock
/// derivations — ONCE per plan/prepare run, shared by every candidate.
///
/// Universal locks (the default) resolve via `--python-version X.Y` (from
/// `python.version`) with no interpreter materialization at all — cheaper
/// (no `ocx package pull` in the plan phase) and, critically, compatible
/// with fully-static interpreter builds that defeat uv's libc inspection
/// (live W4 pilot: "Could not detect a glibc or a musl libc"). Only
/// `universal: false` materializes the pinned `interpreter_package` for an
/// exact-interpreter resolution.
pub(crate) async fn resolve_uv_python(python: &PythonConfig) -> Result<lock_derive::UvPython, MirrorError> {
    let universal = python.lock.as_ref().is_none_or(|lock| lock.universal);
    if universal {
        Ok(lock_derive::UvPython::Version(
            pylock_interpreter_pin(python)?.python_version,
        ))
    } else {
        let interpreter_path = lock_derive::materialize_interpreter(&python.interpreter_package)
            .await
            .map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;
        Ok(lock_derive::UvPython::Interpreter(interpreter_path))
    }
}

/// Derives a single PEP 751 lock for one already-resolved Python selector and
/// one already-known `app_version`. Shared plumbing between the plan-phase
/// candidate loop ([`build_pypi_plan_entries`]) and `prepare.rs`'s standalone
/// (no `--plan`) re-derivation path, both of which otherwise repeat the same
/// `python.lock` defaulting + provenance-timestamp + request assembly.
pub(crate) async fn derive_one_pypi_lock(
    spec: &MirrorSpec,
    uv_python: &lock_derive::UvPython,
    app_version: &str,
    output_path: &Path,
) -> Result<Pylock, MirrorError> {
    let Source::Pypi { index, .. } = &spec.source else {
        unreachable!("derive_one_pypi_lock is only called for source.type: pypi");
    };
    let python = spec
        .python
        .as_ref()
        .expect("validated: python required for source.type 'pypi'");
    let package = spec.source.pylock_app_name(&spec.name);
    let lock_options = python.lock.clone().unwrap_or_else(default_lock_options);
    let generated_at = Utc::now().to_rfc3339();

    let request = lock_derive::DeriveLockRequest {
        python: uv_python,
        package,
        version: app_version,
        index: index.as_deref(),
        options: &lock_options,
        output_path,
        generated_at: &generated_at,
    };
    lock_derive::derive_pylock(&request)
        .await
        .map_err(classify_lock_derive_error)
}

/// Builds the `PlanVersionEntry` list for a `pypi`-sourced spec (design
/// decision A, `plan_python_mirror_v2`).
///
/// [`select_pypi_candidates`] picks the versions worth deriving a lock for
/// (cheap, no subprocess spawns); the Python selector is then resolved ONCE
/// for the whole plan run via [`resolve_uv_python`] (every candidate
/// resolves against the same version/interpreter and index), and each
/// candidate's lock is derived in turn and written under `locks_dir`. The
/// lock-agnostic `build_env_plan_entries` (shared with the `pylock` branch
/// above) does the actual per-(variant, platform) wheel selection once a
/// lock is in hand.
async fn build_pypi_plan_entries(
    spec: &MirrorSpec,
    upstream_versions: &[source::VersionInfo],
    all_tags: &[String],
    version_map: &VersionPlatformMap,
    locks_dir: &Path,
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let python = spec
        .python
        .as_ref()
        .expect("validated: python required for source.type 'pypi'");

    let candidates = select_pypi_candidates(spec, upstream_versions, version_map);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    tokio::fs::create_dir_all(locks_dir).await.map_err(|e| {
        MirrorError::ExecutionFailed(vec![format!(
            "failed to create locks dir '{}': {e}",
            locks_dir.display()
        )])
    })?;

    let uv_python = resolve_uv_python(python).await?;

    let package = spec.source.pylock_app_name(&spec.name);

    let mut entries = Vec::new();
    for version_info in candidates {
        let output_path = locks_dir.join(derived_lock_filename(package, &version_info.version));
        let lock = derive_one_pypi_lock(spec, &uv_python, &version_info.version, &output_path).await?;

        let mut version_entries = build_env_plan_entries(spec, &lock, &version_info.version, all_tags, version_map)?;
        let pylock_path = output_path.to_string_lossy().into_owned();
        for entry in &mut version_entries {
            entry.pylock = Some(pylock_path.clone());
        }
        entries.extend(version_entries);
    }

    Ok(entries)
}

/// Derives the `ocx_python` selection constraints for one `wheels:` platform
/// key: the key's declared libc (or the filter-implied one for plain linux
/// keys — musl iff the effective filter carries `musllinux*` prefixes and no
/// `manylinux*` ones, else gnu) plus the effective filter as the
/// admissibility/ranking list. Floors stay `None` — `select` applies its
/// defaults (`manylinux_2_28`/`musllinux_1_2`); `python.abi` remains the one
/// ABI pin (no per-key override).
pub(crate) fn wheel_target_constraints(wheels: &WheelPatterns, platform: &Platform) -> VariantConstraints {
    let filter = wheels.effective_filter(platform);
    let libc = match spec::libc_feature(platform) {
        Some("libc.musl") => LibcFamily::Musl,
        Some("libc.glibc") => LibcFamily::Gnu,
        _ => {
            let has_musllinux = filter.iter().any(|entry| entry.starts_with("musllinux"));
            let has_manylinux = filter.iter().any(|entry| entry.starts_with("manylinux"));
            if has_musllinux && !has_manylinux {
                LibcFamily::Musl
            } else {
                LibcFamily::Gnu
            }
        }
    };
    VariantConstraints {
        libc: Some(libc),
        min_manylinux: None,
        min_musllinux: None,
        abi: None,
        wheel_priority: Some(filter),
    }
}

/// Builds the interpreter pin from the spec's `python:` block.
pub(crate) fn pylock_interpreter_pin(python: &PythonConfig) -> Result<InterpreterPin, MirrorError> {
    let version = Version::parse(&python.version)
        .ok_or_else(|| MirrorError::PylockError(format!("invalid python.version '{}'", python.version)))?;
    let minor = version
        .minor()
        .ok_or_else(|| MirrorError::PylockError(format!("python.version '{}' needs major.minor", python.version)))?;
    Ok(InterpreterPin {
        python_version: format!("{}.{minor}", version.major()),
        python_full_version: python.version.clone(),
        abi: python.abi.clone(),
        implementation: Implementation::CPython,
    })
}

/// Maps a wheels key's parsed `ocx_lib::oci::Platform` to `ocx_python`'s
/// `TargetPlatform` (os/arch only — the key's `+libc.*` os_features travel
/// through [`wheel_target_constraints`], not this mapping).
pub(crate) fn pylock_target_platform(platform: &Platform, key: &str) -> Result<TargetPlatform, MirrorError> {
    let Platform::Specific { os, arch, .. } = platform else {
        return Err(MirrorError::PylockError(format!(
            "platform key '{key}' must be a concrete os/arch pair for pylock sources"
        )));
    };
    let operating_system = match os {
        OperatingSystem::Linux => TargetOperatingSystem::Linux,
        OperatingSystem::Darwin => TargetOperatingSystem::Darwin,
        OperatingSystem::Windows => TargetOperatingSystem::Windows,
    };
    let architecture = match arch {
        Architecture::Amd64 => TargetArchitecture::Amd64,
        Architecture::Arm64 => TargetArchitecture::Arm64,
    };
    Ok(TargetPlatform {
        operating_system,
        architecture,
    })
}

/// Map filtered resolved versions to plan entries.
///
/// The emitted `version` is the **variant-prefixed normalized tag**
/// (`rv.normalized_version`, e.g. `slim-3.13.9`), not the bare upstream
/// version. The generated workflow keys the whole prepare → test → push chain
/// off this string; if a non-default variant carried only the bare upstream
/// version it would collapse onto the default variant and never be prepared,
/// tested, or pushed.
fn build_version_entries(
    filtered: &[filter::ResolvedVersion],
    all_tags: &[String],
    declared_platform_count: usize,
) -> Vec<PlanVersionEntry> {
    filtered
        .iter()
        .map(|rv| {
            let missing_platforms: Vec<String> = rv.platforms.iter().map(|pa| pa.platform.to_string()).collect();

            // Backfill-partial when the bare upstream version already has at least
            // one platform tile on the registry but some declared platforms remain.
            let version_on_registry = Version::parse(&rv.version)
                .is_some_and(|v| all_tags.iter().any(|t| Version::parse(t).is_some_and(|tv| tv == v)));
            let kind = if version_on_registry && declared_platform_count > missing_platforms.len() {
                PlanVersionKind::BackfillPartial
            } else {
                PlanVersionKind::New
            };

            // Carry the resolved assets so `prepare --plan` never re-runs the
            // source generator (issue #160). After filter_versions,
            // rv.platforms holds exactly the platforms that still need work.
            let assets: Vec<PlanAssetEntry> = rv
                .platforms
                .iter()
                .map(|pa| PlanAssetEntry {
                    platform: pa.platform.to_string(),
                    asset_name: pa.asset_name.clone(),
                    url: pa.url.clone(),
                })
                .collect();

            PlanVersionEntry {
                version: rv.normalized_version.clone(),
                platforms: missing_platforms,
                kind,
                source_version: rv.version.clone(),
                variant: rv.variant.clone(),
                assets,
                pylock: None,
            }
        })
        .collect()
}

/// Plain-text rendering of `PlanReport` — one row per version.
fn print_plan_plain(report: &PlanReport) {
    if !report.has_new {
        println!("nothing to do — target is up to date");
        return;
    }

    println!("target: {}", report.target);
    if let Some(rev) = &report.ocx_mirror_rev {
        println!("ocx_mirror_rev: {rev}");
    }
    println!();

    let versions: Vec<String> = report.versions.iter().map(|v| v.version.clone()).collect();
    let kinds: Vec<String> = report
        .versions
        .iter()
        .map(|v| format!("{:?}", v.kind).to_lowercase())
        .collect();
    let platforms: Vec<String> = report.versions.iter().map(|v| v.platforms.join(", ")).collect();

    // Simple aligned table without pulling in Printer::print_table to avoid
    // mutating the Printer reference across the async boundary.
    let v_w = versions.iter().map(|s| s.len()).max().unwrap_or(7).max(7);
    let k_w = kinds.iter().map(|s| s.len()).max().unwrap_or(4).max(4);

    println!("{:<v_w$}  {:<k_w$}  platforms", "version", "kind", v_w = v_w, k_w = k_w);
    println!("{}", "-".repeat(v_w + k_w + 20));

    for ((v, k), p) in versions.iter().zip(kinds.iter()).zip(platforms.iter()) {
        println!("{:<v_w$}  {:<k_w$}  {}", v, k, p, v_w = v_w, k_w = k_w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── §3.5 S5: ocx-mirror package pipeline plan — unit tests ────────────────────
    //
    // These tests verify the JSON output schema of PlanReport and the types
    // involved. The actual plan computation (source/registry queries) is
    // exercised via integration tests once execute() is implemented.

    /// Test helper: entry with the v2 fields defaulted so schema-shape tests
    /// stay focused on the field under assertion.
    fn entry(version: &str, platforms: &[&str], kind: PlanVersionKind) -> PlanVersionEntry {
        PlanVersionEntry {
            version: version.to_string(),
            platforms: platforms.iter().map(|p| p.to_string()).collect(),
            kind,
            source_version: version.to_string(),
            variant: None,
            assets: vec![],
            pylock: None,
        }
    }

    #[test]
    fn plan_report_serializes_schema_version_2() {
        // §3.5: JSON output format matches design spec §2.2 schema.
        // schema_version 2 since plan entries carry resolved assets (issue #160).
        let report = PlanReport {
            schema_version: 2,
            has_new: true,
            versions: vec![entry("3.29.0", &["linux/amd64", "darwin/arm64"], PlanVersionKind::New)],
            target: "ocx.sh/cmake".to_string(),
            ocx_mirror_rev: Some("abc123def456".to_string()),
        };

        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["schema_version"].as_u64().unwrap(), 2);
        assert!(value["has_new"].as_bool().unwrap());
        assert_eq!(value["target"].as_str().unwrap(), "ocx.sh/cmake");
        assert_eq!(value["ocx_mirror_rev"].as_str().unwrap(), "abc123def456");
    }

    #[test]
    fn plan_report_has_new_false_when_no_versions() {
        // §3.5: Empty source + empty target → has_new: false, versions: []
        let report = PlanReport {
            schema_version: 2,
            has_new: false,
            versions: vec![],
            target: "ocx.sh/cmake".to_string(),
            ocx_mirror_rev: None,
        };

        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert!(!value["has_new"].as_bool().unwrap());
        assert!(value["versions"].as_array().unwrap().is_empty());
        // ocx_mirror_rev: null when None (serde default with Option)
    }

    #[test]
    fn plan_version_kind_new_serializes_as_kebab_case() {
        // §3.5: PlanVersionKind::New → "new" in JSON (kebab-case)
        let value: serde_json::Value =
            serde_json::to_value(entry("3.29.0", &["linux/amd64"], PlanVersionKind::New)).unwrap();
        assert_eq!(value["kind"].as_str().unwrap(), "new");
    }

    #[test]
    fn plan_version_kind_backfill_partial_serializes_as_kebab_case() {
        // §3.5: PlanVersionKind::BackfillPartial → "backfill-partial" in JSON
        let value: serde_json::Value =
            serde_json::to_value(entry("3.28.5", &["linux/arm64"], PlanVersionKind::BackfillPartial)).unwrap();
        assert_eq!(value["kind"].as_str().unwrap(), "backfill-partial");
    }

    #[test]
    fn plan_report_mixed_new_and_backfill_versions() {
        // §3.5: Mixed: 2 versions present in target, 1 new → only 1 in versions[]
        // This test verifies the schema shape for the mixed case.
        let report = PlanReport {
            schema_version: 2,
            has_new: true,
            versions: vec![
                entry("3.29.0", &["linux/amd64", "linux/arm64"], PlanVersionKind::New),
                entry("3.28.5", &["linux/arm64"], PlanVersionKind::BackfillPartial),
            ],
            target: "ocx.sh/cmake".to_string(),
            ocx_mirror_rev: None,
        };

        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        let versions = value["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0]["kind"].as_str().unwrap(), "new");
        assert_eq!(versions[1]["kind"].as_str().unwrap(), "backfill-partial");
        // Partial backfill: only missing platforms listed
        let partial_platforms = versions[1]["platforms"].as_array().unwrap();
        assert_eq!(partial_platforms.len(), 1);
        assert_eq!(partial_platforms[0].as_str().unwrap(), "linux/arm64");
    }

    #[test]
    fn build_version_entries_emits_variant_prefixed_tag() {
        // Regression: a non-default variant must carry its own variant-prefixed
        // normalized tag in the plan. Both default + slim resolve to the same
        // bare upstream version (`3.13.9`); before the fix the plan emitted that
        // bare version for both, so `slim-3.13.9` never became its own matrix
        // leg and was never prepared, tested, or pushed by the workflow.
        use crate::filter::ResolvedVersion;
        use crate::resolver::asset_resolution::ResolvedPlatformAsset;

        let platform: Platform = "linux/amd64".parse().unwrap();
        let asset = || ResolvedPlatformAsset {
            platform: platform.clone(),
            asset_name: "cpython.tar.gz".to_string(),
            url: url::Url::parse("https://example.com/cpython.tar.gz").unwrap(),
        };

        let filtered = vec![
            ResolvedVersion {
                version: "3.13.9".to_string(),
                normalized_version: "3.13.9".to_string(),
                variant: None,
                platforms: vec![asset()],
                is_prerelease: false,
            },
            ResolvedVersion {
                version: "3.13.9".to_string(),
                normalized_version: "slim-3.13.9".to_string(),
                variant: Some("slim".to_string()),
                platforms: vec![asset()],
                is_prerelease: false,
            },
        ];

        let entries = build_version_entries(&filtered, &[], 0);
        let tags: Vec<&str> = entries.iter().map(|e| e.version.as_str()).collect();
        assert_eq!(
            tags,
            vec!["3.13.9", "slim-3.13.9"],
            "plan must emit the variant-prefixed normalized tag, not the bare upstream version"
        );
    }

    #[test]
    fn build_version_entries_carries_resolved_assets() {
        // Regression (issue #160): plan entries must carry the resolved
        // per-platform assets (source_version, variant, asset URLs) so
        // `prepare --plan` consumes the discover crawl instead of re-running
        // the source generator once per matrix leg (N+1 crawls → GraphQL
        // rate-limit exhaustion).
        use crate::filter::ResolvedVersion;
        use crate::resolver::asset_resolution::ResolvedPlatformAsset;

        let platform: Platform = "linux/amd64".parse().unwrap();
        let filtered = vec![ResolvedVersion {
            version: "3.13.9".to_string(),
            normalized_version: "slim-3.13.9_20260610".to_string(),
            variant: Some("slim".to_string()),
            platforms: vec![ResolvedPlatformAsset {
                platform: platform.clone(),
                asset_name: "cpython-slim.tar.gz".to_string(),
                url: url::Url::parse("https://example.com/cpython-slim.tar.gz").unwrap(),
            }],
            is_prerelease: false,
        }];

        let entries = build_version_entries(&filtered, &[], 0);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.source_version, "3.13.9");
        assert_eq!(entry.variant.as_deref(), Some("slim"));
        assert_eq!(entry.assets.len(), 1);
        assert_eq!(entry.assets[0].platform, "linux/amd64");
        assert_eq!(entry.assets[0].asset_name, "cpython-slim.tar.gz");
        assert_eq!(entry.assets[0].url.as_str(), "https://example.com/cpython-slim.tar.gz");

        // Round-trip: prepare deserializes what plan serialized.
        let json = serde_json::to_string(&PlanReport {
            schema_version: 2,
            has_new: true,
            versions: entries,
            target: "ocx.sh/cpython".to_string(),
            ocx_mirror_rev: None,
        })
        .unwrap();
        let parsed: PlanReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.versions[0].assets[0].asset_name, "cpython-slim.tar.gz");
    }

    #[test]
    fn plan_version_entry_omits_pylock_key_when_not_pypi_derived() {
        // `pylock` is set only for `source.type: pypi` entries (the derived
        // lock a version was resolved from); every other source type must
        // leave it absent from the JSON entirely, not `null`.
        let value = serde_json::to_value(entry("3.29.0", &["linux/amd64"], PlanVersionKind::New)).unwrap();
        assert!(
            value.as_object().unwrap().get("pylock").is_none(),
            "expected no 'pylock' key, got: {value}"
        );
    }

    #[test]
    fn plan_cmd_execute_returns_ok_or_err_not_panic() {
        // §3.5: After implementation, execute() must not panic — it must return
        // a Result (Ok or Err). The prior stub-verification assertion (is_err on
        // catch_unwind) is now inverted: catch_unwind succeeds (is_ok) because
        // execute() no longer calls unimplemented!().
        //
        // When the spec file is absent, execute() returns Err(MirrorError::SourceError)
        // with exit code Unavailable — no panic.
        use std::panic;

        let cmd = PlanCmd {
            spec: std::path::PathBuf::from("./nonexistent-mirror.yml"),
            format: None,
            locks_dir: None,
        };
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let _ = rt.block_on(async { cmd.execute(&printer).await });
        }));
        // The closure must NOT panic — catch_unwind returns Ok.
        assert!(
            result.is_ok(),
            "PlanCmd::execute must not panic after implementation; got panic instead of Result"
        );
    }

    // ── W2.2: pylock source — plan-phase wheel selection ────────────────────
    //
    // `build_pylock_plan_entries` is the registry-independent half of the
    // pylock branch (the caller already fetched `all_tags`/`version_map` from
    // the target registry) — the seam that reuses `select_wheels` instead of
    // the regex `resolve_assets` (D1). Tested directly so no live OCI
    // registry is needed; `pipeline plan`'s registry-facing prelude is
    // unchanged for every source type.

    fn pylock_fixture_spec_path() -> std::path::PathBuf {
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pylock.yml"))
    }

    #[tokio::test]
    async fn build_pylock_plan_entries_emits_wheel_assets_per_platform() {
        let spec_path = pylock_fixture_spec_path();
        let spec = spec::load_spec(&spec_path)
            .await
            .expect("fixture spec must load and validate");
        let spec_dir = spec_path.parent().unwrap();

        let upstream_versions = list_upstream_versions(&spec, spec_dir)
            .await
            .expect("pylock source must list the app's locked version");
        assert_eq!(upstream_versions.len(), 1);
        assert_eq!(upstream_versions[0].version, "1.0.0");

        let Source::Pylock { path, .. } = &spec.source else {
            panic!("fixture spec must be source.type: pylock");
        };

        let version_map = VersionPlatformMap::default();
        let entries = build_pylock_plan_entries(&spec, spec_dir, path, &upstream_versions, &[], &version_map)
            .await
            .expect("wheel selection must succeed for the fixture lock");

        assert_eq!(entries.len(), 1, "one declared (unnamed default) variant -> one entry");
        let entry = &entries[0];
        assert_eq!(
            entry.version, "1.0.0",
            "unnamed default variant must produce a bare tag"
        );
        assert_eq!(entry.source_version, "1.0.0");
        assert_eq!(entry.variant, None);
        assert!(matches!(entry.kind, PlanVersionKind::New));

        let mut platforms = entry.platforms.clone();
        platforms.sort();
        assert_eq!(platforms, vec!["linux/amd64".to_string(), "linux/arm64".to_string()]);

        // Two pure-python ("none-any") wheels apply identically on both
        // declared platforms -> N=2 wheel `PlanAssetEntry` per platform.
        assert_eq!(entry.assets.len(), 4, "2 wheels x 2 platforms");
        for platform in ["linux/amd64", "linux/arm64"] {
            let names: Vec<&str> = entry
                .assets
                .iter()
                .filter(|asset| asset.platform == platform)
                .map(|asset| asset.asset_name.as_str())
                .collect();
            assert_eq!(names.len(), 2, "platform {platform} must carry 2 wheel assets");
            assert!(names.contains(&"pycowsay-1.0.0-py3-none-any.whl"));
            assert!(names.contains(&"six-1.16.0-py2.py3-none-any.whl"));
        }

        // Wheel URLs are concrete absolute http(s) — the existing download
        // path (pipeline/download.rs) consumes them as-is.
        for asset in &entry.assets {
            assert_eq!(asset.url.scheme(), "https");
        }
    }

    #[tokio::test]
    async fn build_pylock_plan_entries_skips_already_published_platforms() {
        let spec_path = pylock_fixture_spec_path();
        let spec = spec::load_spec(&spec_path)
            .await
            .expect("fixture spec must load and validate");
        let spec_dir = spec_path.parent().unwrap();

        let upstream_versions = list_upstream_versions(&spec, spec_dir).await.unwrap();
        let Source::Pylock { path, .. } = &spec.source else {
            panic!("fixture spec must be source.type: pylock");
        };

        // Both declared platforms already published for this version — a
        // repeat `pipeline plan` run must report no outstanding work.
        let mut version_map = VersionPlatformMap::default();
        let version = Version::parse("1.0.0").unwrap();
        version_map.add(version.clone(), "linux/amd64".parse().unwrap());
        version_map.add(version, "linux/arm64".parse().unwrap());

        let entries = build_pylock_plan_entries(&spec, spec_dir, path, &upstream_versions, &[], &version_map)
            .await
            .unwrap();
        assert!(
            entries.is_empty(),
            "already-published (version, platform) pairs must be dropped"
        );
    }

    #[tokio::test]
    async fn build_pylock_plan_entries_wraps_select_error_as_pylock_error_exit_65() {
        // A wheel with no tag intersecting the target platform (windows-only
        // build, no marker, requested against linux/amd64) is
        // `SelectError::NoCompatibleWheel` inside `ocx_python::select_wheels`
        // — must surface as `MirrorError::PylockError` (DataError, exit 65),
        // not panic or an unrelated error kind.
        let dir = tempfile::tempdir().unwrap();
        let lock_toml = r#"
lock-version = "1.0"

[[packages]]
name = "windows-only-pkg"
version = "1.0.0"

[[packages.wheels]]
name = "windows_only_pkg-1.0.0-cp313-cp313-win_amd64.whl"
url = "https://example.com/windows_only_pkg-1.0.0-cp313-cp313-win_amd64.whl"
hashes = { sha256 = "3333333333333333333333333333333333333333333333333333333333cccc" }
"#;
        tokio::fs::write(dir.path().join("pylock.toml"), lock_toml)
            .await
            .unwrap();

        let spec_yaml = r#"
name: windows-only-pkg
target:
  registry: ocx.sh
  repository: windows-only-pkg
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;
        let spec_path = dir.path().join("mirror.yml");
        tokio::fs::write(&spec_path, spec_yaml).await.unwrap();
        let spec = spec::load_spec(&spec_path)
            .await
            .expect("fixture spec must load and validate");

        let upstream_versions = list_upstream_versions(&spec, dir.path()).await.unwrap();
        let version_map = VersionPlatformMap::default();

        let err = build_pylock_plan_entries(&spec, dir.path(), "pylock.toml", &upstream_versions, &[], &version_map)
            .await
            .expect_err("a windows-only wheel must fail selection for a linux/amd64 target");

        assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
        assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
    }

    #[tokio::test]
    async fn build_pylock_plan_entries_accepts_pep440_version_beyond_three_components() {
        // Regression (W3.2 first-green-loop blocker): a PyPI app version with
        // more than three numeric components — pycowsay's real `0.0.0.2`, or a
        // calendar version like `2024.1.1.1` — is a valid PEP 440 string but is
        // NOT a parseable `ocx_lib::Version` (a ≤3-component tool-release-tag
        // semver parser). The plan phase must not panic on it: an unparseable
        // tag cannot be in the `Version`-keyed publish map, so it is simply
        // treated as outstanding work.
        let dir = tempfile::tempdir().unwrap();
        let lock_toml = r#"
lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "0.0.0.2"

[[packages.wheels]]
name = "pycowsay-0.0.0.2-py3-none-any.whl"
url = "https://example.com/pycowsay-0.0.0.2-py3-none-any.whl"
hashes = { sha256 = "5c03d8a9c7666ec102aaed4bbd6c7d35228489ce236f95f6e5d079529c6a5050" }
"#;
        tokio::fs::write(dir.path().join("pylock.toml"), lock_toml)
            .await
            .unwrap();

        let spec_yaml = r#"
name: pycowsay
target:
  registry: dev.ocx.sh
  repository: ocx/pycowsay
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/cpython:3.13.1"
wheels:
  linux/amd64: ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;
        let spec_path = dir.path().join("mirror.yml");
        tokio::fs::write(&spec_path, spec_yaml).await.unwrap();
        let spec = spec::load_spec(&spec_path)
            .await
            .expect("fixture spec must load and validate");

        let upstream_versions = list_upstream_versions(&spec, dir.path()).await.unwrap();
        assert_eq!(upstream_versions[0].version, "0.0.0.2");

        let version_map = VersionPlatformMap::default();
        let entries =
            build_pylock_plan_entries(&spec, dir.path(), "pylock.toml", &upstream_versions, &[], &version_map)
                .await
                .expect("a >3-component PEP 440 version must plan without panicking");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version, "0.0.0.2");
        assert_eq!(entries[0].platforms, vec!["linux/amd64".to_string()]);
        assert!(matches!(entries[0].kind, PlanVersionKind::New));
        assert_eq!(entries[0].assets.len(), 1, "one pure-python wheel -> one asset");
    }

    // ── dual-libc wheels keys: one entry, full keys in assets ────────────────

    const DUAL_LIBC_LOCK: &str = r#"
lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"
url = "https://example.com/pycowsay-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"
hashes = { sha256 = "aaaa" }

[[packages.wheels]]
name = "pycowsay-1.0.0-cp313-cp313-musllinux_1_2_x86_64.whl"
url = "https://example.com/pycowsay-1.0.0-cp313-cp313-musllinux_1_2_x86_64.whl"
hashes = { sha256 = "bbbb" }
"#;

    fn dual_libc_spec() -> MirrorSpec {
        let yaml = r#"
name: pycowsay
target:
  registry: ocx.sh
  repository: pycowsay
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  "linux/amd64+libc.glibc": ~
  "linux/amd64+libc.musl": ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn build_env_plan_entries_dual_libc_keys_share_one_entry_and_base_platform() {
        let spec = dual_libc_spec();
        let lock = ocx_python::parse_pylock(DUAL_LIBC_LOCK).unwrap();
        let version_map = VersionPlatformMap::default();

        let entries = build_env_plan_entries(&spec, &lock, "1.0.0", &[], &version_map).unwrap();

        assert_eq!(entries.len(), 1, "env sources emit ONE bare-tag entry");
        let entry = &entries[0];
        assert_eq!(entry.version, "1.0.0", "bare tag, no variant prefix");
        assert_eq!(entry.variant, None);
        assert_eq!(
            entry.platforms,
            vec!["linux/amd64".to_string()],
            "platforms dedupes full keys onto the base CI matrix leg"
        );

        // Each full key selected its libc's wheel; assets carry the FULL key.
        let glibc: Vec<&str> = entry
            .assets
            .iter()
            .filter(|asset| asset.platform == "linux/amd64+libc.glibc")
            .map(|asset| asset.asset_name.as_str())
            .collect();
        assert_eq!(glibc, vec!["pycowsay-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"]);
        let musl: Vec<&str> = entry
            .assets
            .iter()
            .filter(|asset| asset.platform == "linux/amd64+libc.musl")
            .map(|asset| asset.asset_name.as_str())
            .collect();
        assert_eq!(musl, vec!["pycowsay-1.0.0-cp313-cp313-musllinux_1_2_x86_64.whl"]);
    }

    #[test]
    fn build_env_plan_entries_published_dedup_honors_os_features() {
        // The glibc key is already published — only the musl key remains
        // outstanding; the published sibling must NOT mask it.
        let spec = dual_libc_spec();
        let lock = ocx_python::parse_pylock(DUAL_LIBC_LOCK).unwrap();
        let mut version_map = VersionPlatformMap::default();
        version_map.add(
            Version::parse("1.0.0").unwrap(),
            "linux/amd64+libc.glibc".parse().unwrap(),
        );

        let entries = build_env_plan_entries(&spec, &lock, "1.0.0", &["1.0.0".to_string()], &version_map).unwrap();

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.platforms, vec!["linux/amd64".to_string()]);
        assert_eq!(entry.assets.len(), 1, "only the musl key's wheel is planned");
        assert_eq!(entry.assets[0].platform, "linux/amd64+libc.musl");
    }

    // ── wheels-key → selection-constraint derivation ─────────────────────────

    #[test]
    fn wheel_target_constraints_derives_libc_and_filter_per_key() {
        let wheels: WheelPatterns = serde_yaml_ng::from_str(concat!(
            "linux/amd64: ~\n",
            "\"linux/arm64+libc.glibc\": ~\n",
            "\"linux/arm64+libc.musl\": ~\n",
            "windows/amd64: ~\n",
        ))
        .unwrap();
        let by_string = |wanted: &str| {
            wheels
                .filters
                .keys()
                .find(|platform| platform.to_string() == wanted)
                .expect("key present")
        };

        // Plain linux key: default `["any"]` filter, gnu libc (no musllinux
        // prefix in the filter), always a NON-empty wheel_priority.
        let plain = wheel_target_constraints(&wheels, by_string("linux/amd64"));
        assert_eq!(plain.libc, Some(LibcFamily::Gnu));
        assert_eq!(plain.wheel_priority, Some(vec!["any".to_string()]));
        assert_eq!(plain.min_manylinux, None, "floors stay select-defaulted");
        assert_eq!(plain.abi, None, "python.abi remains the one ABI pin");

        let glibc = wheel_target_constraints(&wheels, by_string("linux/arm64+libc.glibc"));
        assert_eq!(glibc.libc, Some(LibcFamily::Gnu));
        assert_eq!(
            glibc.wheel_priority,
            Some(vec!["manylinux".to_string(), "any".to_string()])
        );

        let musl = wheel_target_constraints(&wheels, by_string("linux/arm64+libc.musl"));
        assert_eq!(musl.libc, Some(LibcFamily::Musl));
        assert_eq!(
            musl.wheel_priority,
            Some(vec!["musllinux".to_string(), "any".to_string()])
        );

        let windows = wheel_target_constraints(&wheels, by_string("windows/amd64"));
        assert_eq!(
            windows.libc,
            Some(LibcFamily::Gnu),
            "libc is a linux axis; gnu is inert elsewhere"
        );
        assert_eq!(windows.wheel_priority, Some(vec!["win".to_string(), "any".to_string()]));
    }

    #[test]
    fn wheel_target_constraints_plain_key_with_musllinux_filter_selects_musl_tag_set() {
        // A plain key whose EXPLICIT filter admits only musllinux wheels
        // selects against the musl uv tag set (gnu would exclude them all).
        let wheels: WheelPatterns = serde_yaml_ng::from_str("linux/amd64: [musllinux, any]\n").unwrap();
        let platform = wheels.filters.keys().next().unwrap();

        let constraints = wheel_target_constraints(&wheels, platform);
        assert_eq!(constraints.libc, Some(LibcFamily::Musl));
        assert_eq!(
            constraints.wheel_priority,
            Some(vec!["musllinux".to_string(), "any".to_string()])
        );
    }

    // ── Decision A: pypi source — plan-phase candidate selection + lock derivation ──

    /// Serializes tests that mutate `OCX_BINARY_PIN` / `OCX_MIRROR_UV` —
    /// process-global env vars (mirrors `pipeline::lock_derive`'s own `env_lock`).
    async fn pypi_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    fn write_executable_script(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write script");
        let mut perms = std::fs::metadata(path).expect("stat script").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod script");
    }

    /// Writes a stub `uv` that consumes stdin and writes `body` to the `-o`
    /// argument — same shape as `pipeline::lock_derive`'s own test stub, plus
    /// real uv's pylock output-filename rule: a `-o` basename that does not
    /// start with `pylock.` and end with `.toml` is rejected with uv's own
    /// message (regression guard for the live W4 pilot failure — the earlier,
    /// laxer stub let a non-conforming name through that real uv rejects).
    fn write_uv_stub(path: &std::path::Path, body: &str, exit_code: u32) {
        let script = format!(
            "#!/bin/sh\n\
             cat > /dev/null\n\
             prev=\"\"\n\
             outfile=\"\"\n\
             for arg in \"$@\"; do\n\
             \x20 if [ \"$prev\" = \"-o\" ]; then outfile=\"$arg\"; fi\n\
             \x20 prev=\"$arg\"\n\
             done\n\
             if [ -n \"$outfile\" ]; then\n\
             \x20 base=${{outfile##*/}}\n\
             \x20 case \"$base\" in\n\
             \x20   pylock.toml|pylock.*.toml) ;;\n\
             \x20   *) echo 'error: Expected the output filename to start with `pylock.` and end with `.toml` (e.g., `pylock.toml`, `pylock.dev.toml`)' >&2; exit 2 ;;\n\
             \x20 esac\n\
             \x20 cat > \"$outfile\" <<'LOCKEOF'\n{body}LOCKEOF\n\
             fi\n\
             exit {exit_code}\n"
        );
        write_executable_script(path, &script);
    }

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
wheels:
  linux/amd64: ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn version_info(version: &str, is_prerelease: bool) -> source::VersionInfo {
        source::VersionInfo {
            version: version.to_string(),
            assets: std::collections::HashMap::new(),
            is_prerelease,
        }
    }

    #[test]
    fn locks_dir_default_is_relative_locks() {
        assert_eq!(DEFAULT_LOCKS_DIR, "locks");
    }

    #[test]
    fn select_pypi_candidates_orders_oldest_first_and_applies_new_per_run() {
        let mut spec = pypi_fixture_spec();
        spec.versions = Some(crate::spec::VersionsConfig {
            new_per_run: Some(2),
            ..Default::default()
        });
        let upstream = vec![
            version_info("3.0.0", false),
            version_info("1.0.0", false),
            version_info("2.0.0", false),
        ];
        let version_map = VersionPlatformMap::default();

        let candidates = select_pypi_candidates(&spec, &upstream, &version_map);
        let versions: Vec<&str> = candidates.iter().map(|c| c.version.as_str()).collect();
        // Default backfill (newest_first) with cap=2: oldest-first order among the
        // two highest surviving versions.
        assert_eq!(versions, vec!["2.0.0", "3.0.0"]);
    }

    #[test]
    fn select_pypi_candidates_skips_fully_published_version() {
        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false), version_info("2.0.0", false)];
        let mut version_map = VersionPlatformMap::default();
        version_map.add(Version::parse("1.0.0").unwrap(), "linux/amd64".parse().unwrap());

        let candidates = select_pypi_candidates(&spec, &upstream, &version_map);
        let versions: Vec<&str> = candidates.iter().map(|c| c.version.as_str()).collect();
        assert_eq!(versions, vec!["2.0.0"], "already-published version must be dropped");
    }

    #[test]
    fn select_pypi_candidates_never_panics_on_unparseable_version() {
        // Regression: a PEP 440 version beyond ocx_lib::Version's 3-component
        // parser (e.g. a calendar version) must never panic filter::filter_versions
        // would (its dedup step `.expect()`s a parseable tag) — this is exactly why
        // select_pypi_candidates doesn't reuse it.
        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("2024.1.1.1", false)];
        let version_map = VersionPlatformMap::default();

        let candidates = select_pypi_candidates(&spec, &upstream, &version_map);
        assert_eq!(candidates.len(), 1, "unparseable version kept as outstanding work");
    }

    const PYPI_STUB_LOCK_BODY: &str = r#"lock-version = "1.0"
requires-python = ">=3.9.1"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-py3-none-any.whl"
url = "https://example.com/pycowsay-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;

    /// Writes the stub `ocx` + `uv` scripts `build_pypi_plan_entries` needs, and
    /// sets `OCX_BINARY_PIN`/`OCX_MIRROR_UV` (caller holds `pypi_env_lock`).
    /// Returns the `TempDir` guards so callers keep them alive for the test's
    /// duration.
    fn install_pypi_stubs(uv_lock_body: &str, uv_exit_code: u32) -> (tempfile::TempDir, tempfile::TempDir) {
        let interpreter_root = tempfile::tempdir().unwrap();
        let bin = interpreter_root.path().join("content/python/install/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();

        let scripts_dir = tempfile::tempdir().unwrap();
        let ocx_stub = scripts_dir.path().join("ocx");
        write_executable_script(
            &ocx_stub,
            &format!(
                "#!/bin/sh\necho '{{\"ocx.sh/python/cpython:3.13.1\": \"{}\"}}'\n",
                interpreter_root.path().display()
            ),
        );
        let uv_stub = scripts_dir.path().join("uv");
        write_uv_stub(&uv_stub, uv_lock_body, uv_exit_code);

        // SAFETY: test-only env vars, serialized by `pypi_env_lock()`.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", &ocx_stub);
            std::env::set_var("OCX_MIRROR_UV", &uv_stub);
        }
        (interpreter_root, scripts_dir)
    }

    fn remove_pypi_stubs() {
        // SAFETY: test-only env vars, serialized by `pypi_env_lock()`.
        unsafe {
            std::env::remove_var("OCX_BINARY_PIN");
            std::env::remove_var("OCX_MIRROR_UV");
        }
    }

    #[tokio::test]
    async fn build_pypi_plan_entries_writes_lock_and_references_it_in_the_entry() {
        let _guard = pypi_env_lock().await;
        let _stubs = install_pypi_stubs(PYPI_STUB_LOCK_BODY, 0);

        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir).await;
        remove_pypi_stubs();

        let entries = result.expect("pypi plan entries derive successfully");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version, "1.0.0");
        let pylock_path = entries[0]
            .pylock
            .clone()
            .expect("a pypi-derived entry must carry a pylock path");
        assert!(
            std::path::Path::new(&pylock_path).exists(),
            "the derived lock must exist on disk at the referenced path"
        );
        assert!(pylock_path.contains("pycowsay-1.0.0"));

        // Round-trip through JSON exactly as `plan.json` would carry it.
        let report = PlanReport {
            schema_version: 2,
            has_new: true,
            versions: entries,
            target: "ocx.sh/pycowsay".to_string(),
            ocx_mirror_rev: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: PlanReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.versions[0].pylock.as_deref(), Some(pylock_path.as_str()));
    }

    #[tokio::test]
    async fn build_pypi_plan_entries_reparse_failure_maps_to_data_error_exit_65() {
        // A sdist-only package (no [[packages.wheels]]) parses as valid TOML
        // but is rejected by ocx_python::parse_pylock's fail-closed re-parse —
        // must surface as PylockError (exit 65), not a generic ExecutionFailed (1).
        let _guard = pypi_env_lock().await;
        let bad_body = "lock-version = \"1.0\"\n\n[[packages]]\nname = \"pycowsay\"\nversion = \"1.0.0\"\n";
        let _stubs = install_pypi_stubs(bad_body, 0);

        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir).await;
        remove_pypi_stubs();

        let err = result.expect_err("an unparseable derived lock must fail, not silently succeed");
        assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
        assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
    }

    #[tokio::test]
    async fn build_pypi_plan_entries_universal_mode_never_invokes_ocx() {
        // Regression (live W4 pilot, static-python bug): `uv pip compile
        // --python <path>` fails against a fully-static interpreter ("Could
        // not detect a glibc or a musl libc"). Universal locks (the default)
        // must resolve via `--python-version X.Y` instead — which means the
        // plan phase must NOT materialize the interpreter at all. The ocx
        // stub here hard-fails if invoked, so any reintroduced
        // `materialize_interpreter` call in the universal path turns this red.
        let _guard = pypi_env_lock().await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let ocx_stub = scripts_dir.path().join("ocx");
        write_executable_script(
            &ocx_stub,
            "#!/bin/sh\necho 'ocx must not be invoked for universal lock derivation' >&2\nexit 1\n",
        );
        let uv_stub = scripts_dir.path().join("uv");
        write_uv_stub(&uv_stub, PYPI_STUB_LOCK_BODY, 0);
        // SAFETY: test-only env vars, serialized by `pypi_env_lock()`.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", &ocx_stub);
            std::env::set_var("OCX_MIRROR_UV", &uv_stub);
        }

        let spec = pypi_fixture_spec(); // no lock: block -> universal defaults to true
        let upstream = vec![version_info("1.0.0", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir).await;
        remove_pypi_stubs();

        let entries = result.expect("universal derivation must succeed without touching ocx");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].pylock.is_some());
    }

    #[tokio::test]
    async fn build_pypi_plan_entries_derived_lock_filename_follows_uv_naming_rule() {
        // Regression (live W4 pilot): real `uv pip compile` REJECTS `-o`
        // filenames that do not start with `pylock.` and end with `.toml`
        // ("Expected the output filename to start with `pylock.` ..."). The
        // earlier `{package}-{version}.pylock.toml` shape passed the (then
        // laxer) stub but failed live CI — the stub now enforces the rule, and
        // this locks the emitted shape explicitly.
        let _guard = pypi_env_lock().await;
        let _stubs = install_pypi_stubs(PYPI_STUB_LOCK_BODY, 0);

        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir).await;
        remove_pypi_stubs();

        let entries = result.expect("derivation must succeed with a uv-conforming output filename");
        let pylock_path = entries[0].pylock.as_deref().expect("entry carries a pylock path");
        let filename = std::path::Path::new(pylock_path)
            .file_name()
            .and_then(|name| name.to_str())
            .expect("pylock path has a UTF-8 filename");
        assert!(
            filename.starts_with("pylock.") && filename.ends_with(".toml"),
            "derived lock filename must match uv's `pylock.*.toml` rule, got: {filename}"
        );
    }

    #[tokio::test]
    async fn build_pypi_plan_entries_uv_resolution_failure_maps_to_data_error_exit_65() {
        // W3 acceptance contract: uv-fail→65. A nonzero uv exit (unsolvable
        // requirements, bad package metadata) means this version cannot
        // produce a lock — a data error (PylockError, 65), NOT a generic
        // ExecutionFailed (1), which stays reserved for uv-missing/spawn/
        // timeout failures. The surfaced message must carry uv's stderr.
        let _guard = pypi_env_lock().await;
        let (_interpreter_root, scripts_dir) = install_pypi_stubs("", 0);
        write_executable_script(
            &scripts_dir.path().join("uv"),
            "#!/bin/sh\ncat > /dev/null\necho 'no solution found for pycowsay==1.0.0' >&2\nexit 1\n",
        );

        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir).await;
        remove_pypi_stubs();

        let err = result.expect_err("a nonzero uv exit must fail the plan");
        assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
        assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
        assert!(
            err.to_string().contains("no solution found"),
            "the error must carry uv's stderr, got: {err}"
        );
    }

    #[tokio::test]
    async fn build_pypi_plan_entries_skips_derivation_when_no_candidates() {
        // No uv/ocx stubs installed: if select_pypi_candidates didn't correctly
        // drop the fully-published version, this would fail trying to spawn a
        // real `ocx`/`uv` binary.
        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false)];
        let mut version_map = VersionPlatformMap::default();
        version_map.add(Version::parse("1.0.0").unwrap(), "linux/amd64".parse().unwrap());
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let entries = build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir)
            .await
            .expect("no candidates means no subprocess spawns, so this never touches uv/ocx");
        assert!(entries.is_empty());
        assert!(
            !locks_dir.exists(),
            "locks dir must not even be created when there's nothing to derive"
        );
    }
}
