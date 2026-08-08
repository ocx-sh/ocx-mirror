// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline plan` — compute which versions need work without
//! side-effects. Used by the GHA `discover` job.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::Utc;
use futures::stream::{self, StreamExt, TryStreamExt};
use ocx_lib::cli::DataInterface;
use ocx_lib::log;
use ocx_lib::oci::{Algorithm, Architecture, ClientBuilder, Identifier, OperatingSystem, Platform};
use ocx_lib::package::metadata::Metadata;
use ocx_lib::package::version::Version;
use ocx_lib::publisher::Publisher;
use ocx_python::{
    Implementation, InterpreterPin, LibcFamily, Pylock, PythonTarget, TargetArchitecture, TargetOperatingSystem,
    TargetPlatform, VariantConstraints,
};
use serde::{Deserialize, Serialize};

use crate::command::package::options::OutputFormat;
use crate::command::package::sync::list_upstream_versions;
use crate::error::MirrorError;
use crate::filter;
use crate::filter::pep440_sort_key;
use crate::normalizer;
use crate::pipeline::lock_derive;
use crate::pipeline::orchestrator::{self, ExpectedMetadata, MetadataPlan};
use crate::pipeline::target_registry::{self, PublishedImage};
use crate::resolver;
use crate::resolver::asset_resolution::AssetResolution;
use crate::source;
use crate::spec::{self, BackfillOrder, BinScanMode, LockOptions, MirrorSpec, PythonConfig, Source, WheelPatterns};
use crate::version_platform_map::VersionPlatformMap;

/// Default `--locks-dir` for `pipeline plan` — where derived PEP 751 locks
/// for `source.type: pypi` mirrors are written, relative to the command's
/// working directory (the same directory `plan.json` is written to via
/// stdout redirect in the generated workflow). Shared with `describe.rs`'s
/// catalog autogen, which looks for an already-derived lock in the same
/// place.
pub(crate) const DEFAULT_LOCKS_DIR: &str = "locks";

/// Maximum number of published tags read concurrently by the drift scan.
///
/// Each tile is a small, latency-bound registry round trip (an image index, a
/// child manifest, sometimes a config blob) — not a bulk transfer. Same bound
/// and same reasoning as `LocalIndex::refresh_tags` in `ocx_lib`: enough to
/// resolve a thousand-version mirror in a handful of rounds while capping the
/// simultaneous burst a registry might answer with `429`.
const DRIFT_SCAN_CONCURRENCY: usize = 64;

/// `new` | `backfill-partial` | `metadata-drift` — what kind of work is needed
/// for this version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanVersionKind {
    /// Version not yet present in the target registry.
    New,
    /// Version present for some platforms but missing for others.
    BackfillPartial,
    /// Version fully present, but its published metadata no longer matches
    /// what the spec would produce today. Corrected by re-publishing the
    /// config blob against the existing layers — no download, no upstream
    /// fetch.
    MetadataDrift,
}

impl PlanVersionKind {
    /// The kebab-case wire name, matching the `serde(rename_all)` spelling.
    ///
    /// Kept in step with serde by `plan_version_kind_str_matches_serde`; the
    /// plain renderer used to derive this from `Debug`, which silently
    /// rendered a two-word variant as `metadatadrift`.
    fn as_str(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::BackfillPartial => "backfill-partial",
            Self::MetadataDrift => "metadata-drift",
        }
    }
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
    /// prefix). The whole prepare → test → push chain keys off this string,
    /// so each variant must carry its own tag here.
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
    /// Relative path (from plan.json's directory) of the derived pylock this
    /// entry was resolved from. Set only for `source.type: pypi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pylock: Option<String>,
}

/// Structured output of `ocx-mirror package pipeline plan`.
///
/// JSON shape (schema_version 3 — v3 adds the `metadata-drift` kind and the
/// `has_drift` gate; v2 added `source_version`, `variant`, and resolved
/// `assets` per version entry so `prepare --plan` consumes the discover crawl
/// instead of re-crawling, issue #160):
/// ```json
/// {
///   "schema_version": 3,
///   "has_new": true,
///   "has_drift": false,
///   "versions": [...],
///   "target": "ocx.sh/cmake",
///   "ocx_mirror_rev": "abc123..."
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanReport {
    /// Schema version for forward-compat detection.
    pub schema_version: u32,
    /// `true` when at least one version needs building — `new` or
    /// `backfill-partial`. Deliberately *not* set by `metadata-drift` alone:
    /// the generated workflow gates its download-and-build jobs on this flag,
    /// and a patch has nothing to download.
    pub has_new: bool,
    /// `true` when at least one published version's metadata drifted from the
    /// spec. Gate for the patch job, the counterpart of `has_new`.
    #[serde(default)]
    pub has_drift: bool,
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
    // the failure per source type (pylock/pypi: PylockError/PypiError for
    // malformed lock or index content vs SourceError for an unreachable one;
    // github_release/url_index: always SourceError) — propagate as-is instead
    // of re-stamping every failure as SourceError, which would collapse a data
    // error into an availability one.
    let upstream_versions = list_upstream_versions(spec, spec_dir).await?;

    // Build timestamp (reuse existing normalizer).
    let build_ts = normalizer::build_timestamp(&spec.build_timestamp);

    // Env sources (`pylock`, `pypi`) select wheel SETS (N per platform) via
    // `ocx_python::select_wheels` instead of the regex `resolve_assets`, which
    // assumes exactly one asset per platform and errors (`AmbiguousAsset`) on
    // 2+ — structurally incompatible with wheel sets (D1,
    // plan_pylock_mirror.md). They build their own `PlanVersionEntry` list
    // directly rather than joining the regex path below.
    //
    // Metadata drift is not scanned for them: an env package's metadata is
    // composed from the lock at prepare time (`compose_env`), not resolved
    // from a spec-declared `metadata.json`, so `orchestrator::metadata_plan_for`
    // has nothing to compare a published tile against. `has_drift` is therefore
    // always false here, and the patch job never fires for an env mirror.
    match &spec.source {
        Source::Pylock { path, .. } => {
            let versions =
                build_pylock_plan_entries(spec, spec_dir, path, &upstream_versions, &all_tags, &version_map).await?;
            return Ok(env_plan_report(spec, versions));
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
            return Ok(env_plan_report(spec, versions));
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
    let mut version_entries = build_version_entries(&filtered, &all_tags, declared_platform_count);

    // Output is oldest-first (filter_versions already sorts semver ascending).
    let has_new = !version_entries.is_empty();

    // Published versions whose metadata the spec has since corrected. Appended
    // after the build entries so the download-and-build legs keep leading the
    // plan, and never for a version already scheduled above — one version, one
    // matrix leg.
    let drift_entries =
        detect_metadata_drift(&publisher, &identifier, spec, spec_dir, &all_tags, &version_entries).await?;
    let has_drift = !drift_entries.is_empty();
    version_entries.extend(drift_entries);

    let target = format!("{}/{}", spec.target.registry, spec.target.repository);
    let ocx_mirror_rev = spec.ocx_mirror.as_ref().and_then(|c| c.rev.clone());

    Ok(PlanReport {
        schema_version: 3,
        has_new,
        has_drift,
        versions: version_entries,
        target,
        ocx_mirror_rev,
    })
}

/// The `PlanReport` wrapper for an env source's version entries.
///
/// `has_drift` is always `false`: env metadata is composed from the lock, so
/// there is no spec-declared document to diff a published tile against (see
/// the dispatch in [`build_plan_report`]).
fn env_plan_report(spec: &MirrorSpec, versions: Vec<PlanVersionEntry>) -> PlanReport {
    PlanReport {
        schema_version: 3,
        has_new: !versions.is_empty(),
        has_drift: false,
        versions,
        target: format!("{}/{}", spec.target.registry, spec.target.repository),
        ocx_mirror_rev: spec.ocx_mirror.as_ref().and_then(|c| c.rev.clone()),
    }
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
        .ok_or_else(|| MirrorError::SpecInvalid(vec!["python config is required for env sources".to_string()]))?;
    let interpreter = pylock_interpreter_pin(python)?;
    let wheels_map = spec
        .wheels
        .as_ref()
        .ok_or_else(|| MirrorError::SpecInvalid(vec!["wheels config is required for env sources".to_string()]))?;

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
/// which panics on a real PyPI version string that has more components than
/// that ≤3-component parser accepts (e.g. `0.0.0.2`) or a PEP 440 `uv`-only
/// suffix (`2.0.0.dev0`) — the same reason `build_env_plan_entries` bypasses
/// it for `pylock` (D1, `plan_python_mirror_v2`). This mirrors that
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

    // Total order (see `push::pep440_sort_key`): the pairwise
    // parse-both-or-compare-text comparator this replaces is intransitive, and
    // the resulting order decides which candidates `new_per_run` truncates.
    candidates.sort_by_key(|info| pep440_sort_key(&info.version));

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
/// structured `lock_derive::Error` enum — promote to a real error type if
/// another call site needs to distinguish more sub-failures.
fn classify_lock_derive_error(err: String) -> MirrorError {
    if err.contains("failed to re-parse") || err.contains("uv pip compile exited") {
        MirrorError::PylockError(err)
    } else {
        MirrorError::ExecutionFailed(vec![err])
    }
}

/// The on-disk filename for a derived PEP 751 lock. `uv pip compile` enforces
/// PEP 751 on `-o`: the name must be `pylock.toml` or `pylock.<name>.toml`
/// where `<name>` is non-empty and **contains no dots**. Both the version
/// (`0.0.0.1`) and a dotted distribution name (`zope.interface`) would land
/// dots in `<name>`, so each dot becomes a dash — found by the live W4 pypi
/// pilot, where `pylock.pycowsay-0.0.0.1.toml` failed uv with exit 2.
///
/// The layout stays flat (one directory, one file per version) because nothing
/// parses this name: the plan carries each derived lock's path verbatim in its
/// entry's `pylock` field, `prepare --plan` reads that path, and `describe`
/// picks any lock in the directory by extension. Dashing the dots is therefore
/// lossy but harmless — no caller recovers a version from the filename.
///
/// Shared by the plan-phase candidate loop and `prepare.rs`'s standalone
/// re-derivation so the two sites cannot drift.
pub(crate) fn derived_lock_filename(package: &str, version: &str) -> String {
    let name = format!("{package}-{version}").replace('.', "-");
    format!("pylock.{name}.toml")
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
        return Err(MirrorError::SpecInvalid(vec![
            "lock derivation is only defined for source.type 'pypi'".to_string(),
        ]));
    };
    let python = spec.python.as_ref().ok_or_else(|| {
        MirrorError::SpecInvalid(vec!["python config is required for source.type 'pypi'".to_string()])
    })?;
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
    let python = spec.python.as_ref().ok_or_else(|| {
        MirrorError::SpecInvalid(vec!["python config is required for source.type 'pypi'".to_string()])
    })?;

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

/// Compares every published version's recorded metadata against what the spec
/// would produce today, and emits a `metadata-drift` entry per version that no
/// longer matches.
///
/// This is what makes a metadata fix retroactive: correct `metadata.json` in
/// the mirror repo, and the next cron run re-publishes the affected config
/// blobs against the layers already in the registry. Nothing is deleted and no
/// asset is downloaded.
///
/// A version already scheduled as `new` or `backfill-partial` is skipped — its
/// push writes current metadata anyway for the platforms it touches, and a
/// second entry under the same version string would collide in the workflow's
/// version matrix. A backfilled version whose *existing* platforms drifted is
/// picked up on the following run, once it is fully published.
async fn detect_metadata_drift(
    publisher: &Publisher,
    identifier: &Identifier,
    spec: &MirrorSpec,
    spec_dir: &Path,
    all_tags: &[String],
    scheduled: &[PlanVersionEntry],
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let scheduled: HashSet<&str> = scheduled.iter().map(|entry| entry.version.as_str()).collect();

    // Tags worth scanning: a leaf, not already scheduled, and belonging to a
    // variant the spec still declares metadata for. A spec that declares none
    // cannot publish at all, so there is nothing to compare against.
    let leaves = leaf_versions(all_tags);
    let candidates: Vec<(Version, String, MetadataPlan)> = leaves
        .iter()
        .filter(|(_, tag)| !scheduled.contains(tag.as_str()))
        .filter_map(|(version, tag)| {
            let plan = orchestrator::metadata_plan_for(spec, version)?;
            Some((version.clone(), tag.clone(), plan))
        })
        .collect();

    // Read every candidate tag's child manifests concurrently — this is the
    // bulk of the scan, and it is all latency.
    let images: Vec<PublishedImage> = stream::iter(candidates.iter().map(|(_, tag, _)| tag.clone()))
        .map(|tag| async move { target_registry::fetch_published_images(publisher, identifier, &[tag.as_str()]).await })
        .buffer_unordered(DRIFT_SCAN_CONCURRENCY)
        .try_concat()
        .await?;
    log::info!(
        "Scanned {} published (version, platform) tiles across {} tags for metadata drift",
        images.len(),
        candidates.len(),
    );

    // The expectation depends on the variant and the platform, never on the
    // version — one spec file serves every version of a variant. Memoized so a
    // thousand-version mirror reads those files once per `(variant, platform)`
    // instead of once per published tile.
    let plans: HashMap<&Version, &MetadataPlan> = candidates.iter().map(|(version, _, plan)| (version, plan)).collect();
    let mut expected_metadata: HashMap<(Option<String>, Platform), ExpectedMetadata> = HashMap::new();
    let mut suspects: Vec<(PublishedImage, ExpectedMetadata, BinScanMode)> = Vec::new();

    for image in images {
        let Some(plan) = plans.get(&image.version) else {
            continue;
        };
        let key = (image.version.variant().map(str::to_string), image.platform.clone());
        let expected = match expected_metadata.entry(key) {
            Entry::Occupied(cached) => cached.into_mut(),
            Entry::Vacant(slot) => {
                let resolved =
                    orchestrator::expected_metadata(&plan.config, &image.platform, spec_dir).map_err(|error| {
                        MirrorError::SpecInvalid(vec![format!(
                            "failed to resolve the metadata {} would publish for {}: {error:#}",
                            image.version, image.platform
                        )])
                    })?;
                slot.insert(resolved)
            }
        };
        // Local and free, and the answer for every tile of a healthy mirror —
        // used here only to avoid spawning a network task per clean tile.
        // `image_drifted` below re-runs it as part of the actual decision.
        if !settled_by_digest(&image, &expected.published, plan.bin_scan)? {
            let expected = expected.clone();
            suspects.push((image, expected, plan.bin_scan));
        }
    }

    // Only the tiles the digest could not clear read their config blob, and that
    // read is concurrent too: when every published version predates a metadata
    // field — the case this whole feature exists for — every tile falls through
    // to here, so a sequential phase would hand back exactly the cost the
    // concurrent scan above removes.
    log::info!("{} tiles need a config-blob read to settle", suspects.len());
    let drifted: Vec<Option<(Version, String)>> = stream::iter(suspects)
        .map(|(image, expected, bin_scan)| async move {
            let drifted = image_drift(publisher, identifier, &image, &expected, bin_scan)
                .await?
                .is_some();
            Ok::<_, MirrorError>(drifted.then(|| (image.version, image.platform.to_string())))
        })
        .buffer_unordered(DRIFT_SCAN_CONCURRENCY)
        .try_collect()
        .await?;

    // Regroup: `buffer_unordered` yields out of order, and the plan is
    // oldest-first (BTreeMap keys are Version-ordered).
    let mut by_version: BTreeMap<Version, Vec<String>> = BTreeMap::new();
    for (version, platform) in drifted.into_iter().flatten() {
        by_version.entry(version).or_default().push(platform);
    }

    Ok(by_version
        .into_iter()
        .filter_map(|(version, mut platforms)| {
            platforms.sort();
            let tag = leaves.get(&version)?;
            Some(drift_entry(tag, version.variant(), platforms))
        })
        .collect())
}

/// The metadata a fresh publish would write for `image`, or `None` when the
/// registry already records exactly that.
///
/// The config digest settles the common case without fetching the blob: equal
/// bytes are necessarily equal values. The converse does **not** hold — the
/// published bytes are whatever the `ocx` that wrote them serialized, and a
/// change in JSON key order alone would make every digest differ — so a digest
/// mismatch only promotes the pair to a value comparison. Reporting drift off
/// the digest would republish the entire fleet the first time serialization
/// moved.
///
/// Returns the expectation rather than a bare `bool` because under `bin_scan`
/// the expectation the caller passed in is not the one the comparison ran
/// against: it is missing `binaries`, and the answer is only meaningful after
/// the published claim has been carried into it. Handing that adopted
/// expectation back is what stops `pipeline patch` republishing against the
/// unadopted one and deleting the claim.
pub(crate) async fn image_drift(
    publisher: &Publisher,
    identifier: &Identifier,
    image: &PublishedImage,
    expected: &ExpectedMetadata,
    bin_scan: BinScanMode,
) -> Result<Option<ExpectedMetadata>, MirrorError> {
    if settled_by_digest(image, &expected.published, bin_scan)? {
        return Ok(None);
    }

    let published = target_registry::fetch_published_metadata(publisher, identifier, image).await?;
    let expected = match bin_scan.scans() {
        true => expected
            .adopting_binaries_from(&published, &image.platform)
            .map_err(|error| {
                MirrorError::SpecInvalid(vec![format!(
                    "failed to carry the published binaries of {} ({}) into the expected metadata: {error:#}",
                    image.version, image.platform
                )])
            })?,
        false => expected.clone(),
    };
    Ok(metadata_drifted(&published, &expected.published)?.then_some(expected))
}

/// Whether the config digest alone proves `image` is current.
///
/// Only an *incomplete* expectation has to skip it. A `bin_scan` whose spec
/// declares no `binaries` cannot compute the claim the registry records, so its
/// digest can never match a correctly published tile and trusting it would
/// report drift on every one of them; those go straight to the blob read that
/// adopts the published claim. A spec that declares `binaries` computes the
/// whole document download-free, scanning or not, and skipping the digest there
/// buys nothing: it costs a config-blob GET per tile per run, and turns one
/// unparseable published blob into a `TargetError` that aborts the discover job.
fn settled_by_digest(image: &PublishedImage, expected: &Metadata, bin_scan: BinScanMode) -> Result<bool, MirrorError> {
    let complete = !bin_scan.scans() || expected.binaries().is_some();
    Ok(complete && config_bytes_match(image, expected)?)
}

/// Whether the config blob the registry recorded is byte-identical to the one a
/// fresh publish would write, decided from the descriptor's digest alone.
///
/// `true` proves there is no drift, with no blob fetch. `false` proves nothing:
/// the published bytes are whatever the `ocx` that wrote them serialized, so a
/// key-order change alone flips this while the value is unchanged. The caller
/// must fall back to [`metadata_drifted`] rather than report drift here.
///
/// The push pipeline's backfill cascade repair asks the same question for the
/// opposite reason: it re-pushes a published tile only to move the rolling tags,
/// so it needs the re-push to be manifest-identical, and `false` — "this build
/// would write different bytes" — is exactly the condition under which it must
/// not run.
pub(crate) fn config_bytes_match(image: &PublishedImage, expected: &Metadata) -> Result<bool, MirrorError> {
    let bytes = serde_json::to_vec(expected)
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("failed to serialize metadata: {error}")]))?;
    Ok(image.config.digest == Algorithm::Sha256.hash(&bytes).to_string())
}

/// Compares two metadata documents by value.
///
/// Both sides are re-serialized from the deserialized form, so key order and
/// whitespace of the published bytes cannot register as a difference. A byte
/// comparison here would report drift for every version published by a
/// different `ocx` and republish the whole mirror on the next cron run.
fn metadata_drifted(published: &Metadata, expected: &Metadata) -> Result<bool, MirrorError> {
    let serialize = |metadata: &Metadata| {
        serde_json::to_value(metadata)
            .map_err(|error| MirrorError::ExecutionFailed(vec![format!("failed to serialize metadata: {error}")]))
    };
    Ok(serialize(published)? != serialize(expected)?)
}

/// Published version tags that are not a cascade alias of another published
/// tag, keyed by version so the result is oldest-first.
///
/// `3.29.0_20260610` cascades to `3.29.0`, `3.29`, and `3` — four tags over one
/// set of child manifests. Scanning the aliases too would schedule the same
/// patch up to four times; patching the leaf re-cascades them anyway.
pub(crate) fn leaf_versions(all_tags: &[String]) -> BTreeMap<Version, String> {
    let mut published: BTreeMap<Version, String> = BTreeMap::new();
    for tag in all_tags {
        if let Some(version) = Version::parse(tag) {
            published.insert(version, tag.clone());
        }
    }

    let aliases: HashSet<Version> = published
        .keys()
        .flat_map(|version| std::iter::successors(version.parent(), Version::parent))
        .collect();
    published.retain(|version, _| !aliases.contains(version));
    published
}

/// A `metadata-drift` plan entry for an already-published tag.
///
/// `source_version` and `assets` stay empty by construction: a metadata patch
/// re-references the published layers by digest and never touches the upstream
/// source, and a normalized tag cannot be reversed into the upstream version
/// string it was built from.
fn drift_entry(tag: &str, variant: Option<&str>, platforms: Vec<String>) -> PlanVersionEntry {
    PlanVersionEntry {
        version: tag.to_string(),
        platforms,
        kind: PlanVersionKind::MetadataDrift,
        source_version: String::new(),
        variant: variant.map(str::to_string),
        assets: Vec::new(),
        pylock: None,
    }
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
    if report.versions.is_empty() {
        println!("nothing to do — target is up to date");
        return;
    }

    println!("target: {}", report.target);
    if let Some(rev) = &report.ocx_mirror_rev {
        println!("ocx_mirror_rev: {rev}");
    }
    println!();

    let versions: Vec<String> = report.versions.iter().map(|v| v.version.clone()).collect();
    let kinds: Vec<String> = report.versions.iter().map(|v| v.kind.as_str().to_string()).collect();
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

    // Drift is reported, never auto-scheduled: patching is a human decision, so
    // the reader of a plan needs the command as well as the rows.
    let drifted = report
        .versions
        .iter()
        .filter(|v| matches!(v.kind, PlanVersionKind::MetadataDrift))
        .count();
    if drifted > 0 {
        println!();
        println!(
            "{drifted} published version(s) carry metadata the spec has since changed — \
             correct them with `ocx-mirror package pipeline patch --metadata-only`"
        );
    }
}

#[cfg(test)]
#[path = "plan/tests.rs"]
mod tests;
