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
mod tests {
    use ocx_lib::oci::{Algorithm, Descriptor, Digest, Platform};

    use super::*;

    // ── bin_scan: the drift comparison against a claim it cannot recompute ──
    //
    // `plan` and `patch` are download-free by design, so neither can run the
    // scan that produces `binaries`. Every assertion below defends the same
    // property from a different side: the expectation must adopt the published
    // claim, or a scanned mirror drifts on every run and each patch republishes
    // the claim away.

    /// The metadata a mirror's spec file declares — no `binaries`, because a
    /// scanned one is never written into the spec.
    fn spec_expectation() -> ExpectedMetadata {
        let authoring = serde_json::from_slice(br#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#)
            .expect("metadata fixture parses");
        ExpectedMetadata::render(authoring, &scan_platform()).expect("the fixture renders both projections")
    }

    fn scan_platform() -> Platform {
        "linux/amd64".parse().expect("valid platform")
    }

    /// What the registry records for a tile a `bin_scan` mirror published: the
    /// spec's metadata plus the scanned claim.
    fn published_with_binaries() -> Metadata {
        serde_json::from_slice(
            br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake","ctest"]}"#,
        )
        .expect("published fixture parses")
    }

    fn image_recording(config_digest: &str) -> PublishedImage {
        PublishedImage {
            version: Version::parse("3.29.0").expect("valid version"),
            platform: scan_platform(),
            manifest_digest: Digest::Sha256("b".repeat(64)),
            config: Descriptor {
                media_type: "application/vnd.ocx.package.metadata.v1+json".to_string(),
                digest: config_digest.to_string(),
                size: 42,
                urls: None,
                annotations: None,
                artifact_type: None,
            },
            layers: vec![],
        }
    }

    /// A correctly published `bin_scan` tile must never be settled from its
    /// config digest.
    ///
    /// The expectation resolved from the spec structurally cannot carry
    /// `binaries`, so its digest can only match a tile that has none. Trusting
    /// the digest therefore reports drift on every correctly published tile,
    /// forever — and `patch` acting on that republishes the claim away.
    #[test]
    fn only_an_incomplete_bin_scan_tile_skips_its_config_digest() {
        let expected = spec_expectation();
        let bytes = serde_json::to_vec(&expected.published).expect("serializes");
        // The strongest possible case for a skip: the digests are identical.
        let image = image_recording(&Algorithm::Sha256.hash(&bytes).to_string());

        assert!(
            settled_by_digest(&image, &expected.published, BinScanMode::Off).expect("digest compares"),
            "control: without a bin_scan an identical config digest must settle the tile",
        );
        for mode in [BinScanMode::Auto, BinScanMode::Verify] {
            assert!(
                !settled_by_digest(&image, &expected.published, mode).expect("digest compares"),
                "{mode:?} on a spec declaring no binaries must fall through to the blob read that \
                 adopts the published claim, even on an identical config digest",
            );
        }

        // A spec that hand-declares `binaries` computes the whole document
        // download-free, scanning or not — mirror-kitware is exactly this. Making
        // it skip the digest anyway costs a config-blob GET per tile per cron run
        // and turns one unparseable blob into an aborted discover job.
        let declared: ocx_lib::package::metadata::authoring::AuthoringMetadata = serde_json::from_slice(
            br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake","ctest"]}"#,
        )
        .expect("declared fixture parses");
        let complete = ExpectedMetadata::render(declared, &scan_platform()).expect("renders");
        let bytes = serde_json::to_vec(&complete.published).expect("serializes");
        let image = image_recording(&Algorithm::Sha256.hash(&bytes).to_string());

        for mode in [BinScanMode::Off, BinScanMode::Auto, BinScanMode::Verify] {
            assert!(
                settled_by_digest(&image, &complete.published, mode).expect("digest compares"),
                "{mode:?} with a declared claim is fully computable and must settle on the digest",
            );
        }
    }

    /// The published claim must reach both projections of the expectation.
    ///
    /// `published` decides drift; `sidecar_json` is what `patch` pushes. A fix
    /// that adopted only the first would still delete the claim the moment any
    /// unrelated field drifted.
    #[test]
    fn adopting_carries_the_published_binaries_into_both_projections() {
        let expected = spec_expectation();
        assert!(
            expected.published.binaries().is_none(),
            "the spec-resolved expectation must start without binaries, or this test proves nothing",
        );

        let adopted = expected
            .adopting_binaries_from(&published_with_binaries(), &scan_platform())
            .expect("adoption renders");

        assert_eq!(
            adopted.published.binaries().map(|binaries| binaries.len()),
            Some(2),
            "the drift comparison must see the published claim",
        );
        assert!(
            adopted.sidecar_json.contains("cmake") && adopted.sidecar_json.contains("ctest"),
            "the sidecar patch republishes with must carry the claim too, or the push deletes it: {}",
            adopted.sidecar_json,
        );
    }

    /// The whole point, stated as the comparison itself: a tile differing from
    /// the spec *only* by its scanned `binaries` is current, not drifted.
    #[test]
    fn a_tile_differing_only_by_its_scanned_binaries_is_current() {
        let published = published_with_binaries();
        let expected = spec_expectation();

        assert!(
            metadata_drifted(&published, &expected.published).expect("compares"),
            "control: unadopted, the published claim reads as drift — this is the bug being prevented",
        );
        assert!(
            !metadata_drifted(
                &published,
                &expected
                    .adopting_binaries_from(&published, &scan_platform())
                    .expect("adoption renders")
                    .published,
            )
            .expect("compares"),
            "with the claim adopted, the tile must read as current",
        );
    }

    /// A `binaries` list the *spec* declares must never be adopted away.
    ///
    /// Adoption exists for the claim only the artifact knows — a scanned one.
    /// Applying it to a hand-written list rewrites the expectation to whatever
    /// the registry already holds, so that field can never drift: a maintainer
    /// correcting a wrong declared list gets silence, `plan` reports nothing,
    /// and the fix never reaches the published versions. Includes `verify`,
    /// which checks the declaration against the tree rather than replacing it.
    #[test]
    fn a_spec_declared_binaries_claim_is_never_adopted_away() {
        let declared: ocx_lib::package::metadata::authoring::AuthoringMetadata = serde_json::from_slice(
            br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake","ctest"]}"#,
        )
        .expect("declared fixture parses");
        let expected = ExpectedMetadata::render(declared, &scan_platform()).expect("renders");

        // What the registry still records: the shorter, wrong list the spec was
        // just corrected away from.
        let published: Metadata = serde_json::from_slice(
            br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake"]}"#,
        )
        .expect("published fixture parses");

        let adopted = expected
            .adopting_binaries_from(&published, &scan_platform())
            .expect("adoption renders");

        assert_eq!(
            adopted.published.binaries().map(|binaries| binaries.len()),
            Some(2),
            "the spec's declared list must survive adoption untouched",
        );
        assert!(
            metadata_drifted(&published, &adopted.published).expect("compares"),
            "a corrected hand-written list must report drift so the fix can land",
        );
    }

    /// Adoption must not invent a claim. A mirror that turns `bin_scan` on
    /// publishes `binaries` from the next push onward; until then the published
    /// tiles have none, and reporting them as current would hide real drift on
    /// every other field.
    #[test]
    fn adopting_from_a_tile_without_binaries_changes_nothing() {
        let expected = spec_expectation();
        let published: Metadata =
            serde_json::from_slice(br#"{"type":"bundle","version":1,"env":[]}"#).expect("published fixture parses");

        let adopted = expected
            .adopting_binaries_from(&published, &scan_platform())
            .expect("adoption renders");

        assert!(
            adopted.published.binaries().is_none(),
            "nothing to adopt must leave the expectation untouched",
        );
        assert!(
            metadata_drifted(&published, &adopted.published).expect("compares"),
            "a tile that really has drifted (strip_components) must still report drift",
        );
    }

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
    fn plan_report_serializes_schema_version_3() {
        // §3.5: JSON output format matches design spec §2.2 schema.
        // schema_version 3 since the plan carries the metadata-drift kind and
        // the has_drift gate beside has_new.
        let report = PlanReport {
            schema_version: 3,
            has_new: true,
            has_drift: false,
            versions: vec![entry("3.29.0", &["linux/amd64", "darwin/arm64"], PlanVersionKind::New)],
            target: "ocx.sh/cmake".to_string(),
            ocx_mirror_rev: Some("abc123def456".to_string()),
        };

        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["schema_version"].as_u64().unwrap(), 3);
        assert!(value["has_new"].as_bool().unwrap());
        assert!(!value["has_drift"].as_bool().unwrap());
        assert_eq!(value["target"].as_str().unwrap(), "ocx.sh/cmake");
        assert_eq!(value["ocx_mirror_rev"].as_str().unwrap(), "abc123def456");
    }

    #[test]
    fn plan_report_has_new_false_when_no_versions() {
        // §3.5: Empty source + empty target → has_new: false, versions: []
        let report = PlanReport {
            schema_version: 3,
            has_new: false,
            has_drift: false,
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
            schema_version: 3,
            has_new: true,
            has_drift: false,
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
            schema_version: 3,
            has_new: true,
            has_drift: false,
            versions: entries,
            target: "ocx.sh/cpython".to_string(),
            ocx_mirror_rev: None,
        })
        .unwrap();
        let parsed: PlanReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.versions[0].assets[0].asset_name, "cpython-slim.tar.gz");
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

    // ── metadata drift (ocx-mirror#9) ─────────────────────────────────────
    //
    // The comparison decides whether an already-published version gets
    // re-pushed. A false positive republishes the fleet and re-points every
    // version tag; a false negative leaves the stale metadata that made
    // `pipeline patch` necessary in the first place. So each case below is
    // asserted in both directions.

    fn metadata(json: &str) -> Metadata {
        serde_json::from_str(json).expect("metadata fixture parses")
    }

    /// A published `metadata.json` from before `binaries` existed, and the same
    /// document as the spec writes it today. This is the drift the pilots have.
    const WITHOUT_BINARIES: &str = r#"{"type":"bundle","version":1,"strip_components":1}"#;
    const WITH_BINARIES: &str = r#"{"type":"bundle","version":1,"strip_components":1,"binaries":["cmake"]}"#;

    #[test]
    fn identical_metadata_is_not_drift() {
        // Green half of the pair: the steady state of every healthy mirror. If
        // this ever reds, the next cron run republishes every published version
        // of every mirror.
        let published = metadata(WITH_BINARIES);
        let expected = metadata(WITH_BINARIES);
        assert!(
            !metadata_drifted(&published, &expected).expect("comparison succeeds"),
            "identical metadata must not report drift"
        );
    }

    #[test]
    fn a_changed_field_is_drift() {
        // Red half: the spec gained `binaries` after the version was published.
        let published = metadata(WITHOUT_BINARIES);
        let expected = metadata(WITH_BINARIES);
        assert!(
            metadata_drifted(&published, &expected).expect("comparison succeeds"),
            "a metadata field added by the spec must report drift"
        );
    }

    #[test]
    fn key_order_does_not_fabricate_drift() {
        // The reason the comparison is by value and not by bytes. Both
        // documents describe the same package; only the key order differs, as
        // it does across the ocx versions that wrote the fleet's config blobs.
        const REORDERED: &str = r#"{"binaries":["cmake"],"strip_components":1,"version":1,"type":"bundle"}"#;

        // Guard: if these ever became byte-equal the test would pass without
        // exercising anything — a byte comparison must be able to red here.
        assert_ne!(
            WITH_BINARIES.as_bytes(),
            REORDERED.as_bytes(),
            "the fixtures must differ as bytes, or this test proves nothing"
        );

        assert!(
            !metadata_drifted(&metadata(REORDERED), &metadata(WITH_BINARIES)).expect("comparison succeeds"),
            "key order alone must not report drift — it would republish the whole fleet"
        );
    }

    /// A published image whose config descriptor records `blob` verbatim —
    /// exactly what the registry returns for bytes some earlier `ocx` wrote.
    fn published_image(blob: &str) -> PublishedImage {
        PublishedImage {
            version: Version::parse("3.29.0").expect("valid version"),
            platform: "linux/amd64".parse().expect("valid platform"),
            manifest_digest: Algorithm::Sha256.hash(b"manifest"),
            config: ocx_lib::oci::Descriptor {
                media_type: "application/vnd.sh.ocx.package.v1+json".to_string(),
                digest: Algorithm::Sha256.hash(blob.as_bytes()).to_string(),
                size: blob.len() as i64,
                urls: None,
                artifact_type: None,
                annotations: None,
            },
            layers: vec![],
        }
    }

    #[test]
    fn matching_config_digest_settles_it_without_a_blob_fetch() {
        // The steady state: the recorded blob is the byte-for-byte output of
        // serializing the expected metadata, so the fast path answers "no
        // drift" and `plan` never fetches the blob.
        let expected = metadata(WITH_BINARIES);
        let recorded = String::from_utf8(serde_json::to_vec(&expected).expect("serializes")).expect("utf-8");
        let image = published_image(&recorded);

        assert!(
            config_bytes_match(&image, &expected).expect("digest compares"),
            "identical config bytes must short-circuit the comparison"
        );
    }

    #[test]
    fn reordered_config_bytes_do_not_short_circuit_as_drift() {
        // The asymmetry the fast path rests on. These bytes describe the same
        // package but were written with a different key order, so their digest
        // differs — `config_bytes_match` must answer "cannot tell" (false) and
        // hand the pair to the value comparison, which finds no drift. A fast
        // path that reported drift on a digest mismatch would republish every
        // version the fleet holds.
        const REORDERED: &str = r#"{"binaries":["cmake"],"strip_components":1,"version":1,"type":"bundle"}"#;
        let expected = metadata(WITH_BINARIES);
        let image = published_image(REORDERED);

        assert!(
            !config_bytes_match(&image, &expected).expect("digest compares"),
            "a different serialization must not match by digest, or this test proves nothing"
        );
        assert!(
            !metadata_drifted(&metadata(REORDERED), &expected).expect("comparison succeeds"),
            "the fallback comparison must clear it — same value, different bytes"
        );
    }

    #[test]
    fn drift_entry_names_the_version_and_platforms() {
        let entry = drift_entry(
            "slim-3.13.9_20260610",
            Some("slim"),
            vec!["darwin/arm64".to_string(), "linux/amd64".to_string()],
        );

        let value: serde_json::Value = serde_json::to_value(&entry).expect("entry serializes");
        assert_eq!(value["kind"].as_str().unwrap(), "metadata-drift");
        assert_eq!(value["version"].as_str().unwrap(), "slim-3.13.9_20260610");
        assert_eq!(
            value["platforms"].as_array().unwrap(),
            &vec![
                serde_json::Value::from("darwin/arm64"),
                serde_json::Value::from("linux/amd64")
            ]
        );
        assert_eq!(value["variant"].as_str().unwrap(), "slim");
        // A patch downloads nothing: no source version, no resolved assets.
        assert_eq!(value["source_version"].as_str().unwrap(), "");
        assert!(value["assets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn drift_entry_survives_the_discover_projection() {
        // The generated workflow projects `[.versions[] | {version, platforms,
        // kind}]` into the job matrix (generate/templates/workflow.yml). An
        // added kind must keep every projected field present and non-null.
        let report = PlanReport {
            schema_version: 3,
            has_new: false,
            has_drift: true,
            versions: vec![drift_entry("3.29.0", None, vec!["linux/amd64".to_string()])],
            target: "ocx.sh/cmake".to_string(),
            ocx_mirror_rev: None,
        };

        let value: serde_json::Value = serde_json::to_value(&report).expect("report serializes");
        assert!(value["has_drift"].as_bool().unwrap());
        assert!(
            !value["has_new"].as_bool().unwrap(),
            "drift alone must not set has_new — the build jobs gate on it and a patch has nothing to build"
        );
        let projected = &value["versions"][0];
        assert!(projected["version"].is_string());
        assert!(projected["platforms"].is_array());
        assert_eq!(projected["kind"].as_str().unwrap(), "metadata-drift");
    }

    #[test]
    fn plan_version_kind_str_matches_serde() {
        // `as_str` renders the plain table; serde renders plan.json. Two
        // spellings of one wire name — this is the lock that keeps them equal.
        for kind in [
            PlanVersionKind::New,
            PlanVersionKind::BackfillPartial,
            PlanVersionKind::MetadataDrift,
        ] {
            let serialized = serde_json::to_value(&kind).expect("kind serializes");
            assert_eq!(
                serialized.as_str().unwrap(),
                kind.as_str(),
                "as_str must match the serde spelling for {kind:?}"
            );
        }
    }

    #[test]
    fn leaf_versions_drops_cascade_aliases() {
        // `3.29.0_20260610` cascades to `3.29.0`, `3.29`, `3` over the same
        // child manifests. Scanning the aliases would schedule the same patch
        // four times.
        let tags: Vec<String> = ["3.29.0_20260610", "3.29.0", "3.29", "3", "latest"]
            .iter()
            .map(|t| t.to_string())
            .collect();

        let leaves = leaf_versions(&tags);
        let leaves: Vec<&str> = leaves.values().map(String::as_str).collect();
        assert_eq!(leaves, vec!["3.29.0_20260610"]);
    }

    #[test]
    fn leaf_versions_keeps_a_mirror_without_build_timestamps() {
        // Not every mirror stamps a build timestamp; there the patch version
        // itself is the leaf, and dropping it would disable drift detection
        // entirely for that mirror.
        let tags: Vec<String> = ["3.29.0", "3.28.5", "3.29", "3.28", "3", "latest"]
            .iter()
            .map(|t| t.to_string())
            .collect();

        let leaves = leaf_versions(&tags);
        let leaves: Vec<&str> = leaves.values().map(String::as_str).collect();
        // BTreeMap keys are Version-ordered, so the output is oldest first.
        assert_eq!(leaves, vec!["3.28.5", "3.29.0"]);
    }

    #[test]
    fn leaf_versions_keeps_variants_apart() {
        // `slim-3.13.9` is not an alias of `3.13.9` — different variant, its own
        // metadata block, its own patch.
        let tags: Vec<String> = ["3.13.9", "slim-3.13.9", "3.13", "slim-3.13"]
            .iter()
            .map(|t| t.to_string())
            .collect();

        let leaves = leaf_versions(&tags);
        let leaves: Vec<&str> = leaves.values().map(String::as_str).collect();
        assert_eq!(leaves.len(), 2, "got {leaves:?}");
        assert!(leaves.contains(&"3.13.9"));
        assert!(leaves.contains(&"slim-3.13.9"));
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

    /// Serializes tests that mutate `OCX_BINARY_PIN` / `OCX_MIRROR_UV`.
    ///
    /// The crate-wide lock, shared with `pipeline push`'s tests: both modules
    /// pin `OCX_BINARY_PIN` at their own stand-in binary, and a module-local
    /// lock leaves a push test resolving *this* module's `uv` stub. Held
    /// across a `block_on` rather than an `await`, which is why these tests are
    /// `#[test]` + an explicit runtime instead of `#[tokio::test]`.
    fn pypi_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        crate::test_support::ocx_env_lock()
    }

    /// Drive one async body to completion under the current thread's runtime.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(future)
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
             \x20 name=${{base#pylock.}}\n\
             \x20 name=${{name%.toml}}\n\
             \x20 case \"$base\" in\n\
             \x20   pylock.toml) ;;\n\
             \x20   pylock.*.toml)\n\
             \x20     case \"$name\" in\n\
             \x20       \"\"|*.*) echo \"error: Expected the output filename to be \\`pylock.toml\\` or \\`pylock.<name>.toml\\`, where \\`<name>\\` is non-empty and contains no dots; found \\`$base\\`\" >&2; exit 2 ;;\n\
             \x20     esac\n\
             \x20     ;;\n\
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

    #[test]
    fn build_pypi_plan_entries_writes_lock_and_references_it_in_the_entry() {
        let _guard = pypi_env_lock();
        let _stubs = install_pypi_stubs(PYPI_STUB_LOCK_BODY, 0);

        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = block_on(build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir));
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
        // Dots are dashed out of the `<name>` segment — uv rejects a dotted one.
        assert!(pylock_path.contains("pylock.pycowsay-1-0-0.toml"), "got: {pylock_path}");

        // Round-trip through JSON exactly as `plan.json` would carry it.
        let report = PlanReport {
            schema_version: 3,
            has_new: true,
            has_drift: false,
            versions: entries,
            target: "ocx.sh/pycowsay".to_string(),
            ocx_mirror_rev: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: PlanReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.versions[0].pylock.as_deref(), Some(pylock_path.as_str()));
    }

    #[test]
    fn build_pypi_plan_entries_reparse_failure_maps_to_data_error_exit_65() {
        // A sdist-only package (no [[packages.wheels]]) parses as valid TOML
        // but is rejected by ocx_python::parse_pylock's fail-closed re-parse —
        // must surface as PylockError (exit 65), not a generic ExecutionFailed (1).
        let _guard = pypi_env_lock();
        let bad_body = "lock-version = \"1.0\"\n\n[[packages]]\nname = \"pycowsay\"\nversion = \"1.0.0\"\n";
        let _stubs = install_pypi_stubs(bad_body, 0);

        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("1.0.0", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = block_on(build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir));
        remove_pypi_stubs();

        let err = result.expect_err("an unparseable derived lock must fail, not silently succeed");
        assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
        assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
    }

    #[test]
    fn build_pypi_plan_entries_universal_mode_never_invokes_ocx() {
        // Regression (live W4 pilot, static-python bug): `uv pip compile
        // --python <path>` fails against a fully-static interpreter ("Could
        // not detect a glibc or a musl libc"). Universal locks (the default)
        // must resolve via `--python-version X.Y` instead — which means the
        // plan phase must NOT materialize the interpreter at all. The ocx
        // stub here hard-fails if invoked, so any reintroduced
        // `materialize_interpreter` call in the universal path turns this red.
        let _guard = pypi_env_lock();
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

        let result = block_on(build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir));
        remove_pypi_stubs();

        let entries = result.expect("universal derivation must succeed without touching ocx");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].pylock.is_some());
    }

    #[test]
    fn derived_lock_filename_name_segment_carries_no_dots() {
        // Regression (live W4 pypi pilot, mirror-pypi run 30874908847): uv
        // enforces PEP 751 on `-o`, and `<name>` in `pylock.<name>.toml` must
        // be non-empty and dot-free. A dotted version went straight through
        // into `<name>` and uv exited 2:
        //   error: Expected the output filename to be `pylock.toml` or
        //   `pylock.<name>.toml`, where `<name>` is non-empty and contains no
        //   dots; found `pylock.pycowsay-0.0.0.1.toml`
        for (package, version) in [
            ("pycowsay", "0.0.0.1"),
            ("black", "26.5.1"),
            // A dotted distribution name (`zope.interface`) must be sanitized
            // on the package side too, not just the version side.
            ("zope.interface", "7.0"),
        ] {
            let filename = derived_lock_filename(package, version);
            let name = filename
                .strip_prefix("pylock.")
                .and_then(|rest| rest.strip_suffix(".toml"))
                .unwrap_or_else(|| panic!("filename must be `pylock.<name>.toml`, got: {filename}"));
            assert!(!name.is_empty(), "`<name>` must be non-empty, got: {filename}");
            assert!(!name.contains('.'), "uv rejects a dotted `<name>`; got: {filename}");
        }
    }

    #[test]
    fn build_pypi_plan_entries_derived_lock_filename_follows_uv_naming_rule() {
        // Regression (live W4 pilot): real `uv pip compile` REJECTS `-o`
        // filenames outside `pylock.toml` / `pylock.<name>.toml` with a
        // non-empty, dot-free `<name>`. The earlier
        // `{package}-{version}.pylock.toml` and `pylock.{package}-{version}.toml`
        // shapes both passed a laxer stub but failed live CI — the stub now
        // enforces uv's full rule, so this exercises it end to end on the
        // pilot's own dotted version.
        let _guard = pypi_env_lock();
        let _stubs = install_pypi_stubs(PYPI_STUB_LOCK_BODY, 0);

        let spec = pypi_fixture_spec();
        let upstream = vec![version_info("0.0.0.1", false)];
        let version_map = VersionPlatformMap::default();
        let locks_root = tempfile::tempdir().unwrap();
        let locks_dir = locks_root.path().join("locks");

        let result = block_on(build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir));
        remove_pypi_stubs();

        let entries = result.expect("derivation must succeed with a uv-conforming output filename");
        let pylock_path = entries[0].pylock.as_deref().expect("entry carries a pylock path");
        let filename = std::path::Path::new(pylock_path)
            .file_name()
            .and_then(|name| name.to_str())
            .expect("pylock path has a UTF-8 filename");
        let name = filename
            .strip_prefix("pylock.")
            .and_then(|rest| rest.strip_suffix(".toml"))
            .unwrap_or_else(|| {
                panic!("derived lock filename must match uv's `pylock.<name>.toml` rule, got: {filename}")
            });
        assert!(
            !name.is_empty() && !name.contains('.'),
            "uv requires a non-empty, dot-free `<name>`; got: {filename}"
        );
    }

    #[test]
    fn build_pypi_plan_entries_uv_resolution_failure_maps_to_data_error_exit_65() {
        // W3 acceptance contract: uv-fail→65. A nonzero uv exit (unsolvable
        // requirements, bad package metadata) means this version cannot
        // produce a lock — a data error (PylockError, 65), NOT a generic
        // ExecutionFailed (1), which stays reserved for uv-missing/spawn/
        // timeout failures. The surfaced message must carry uv's stderr.
        let _guard = pypi_env_lock();
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

        let result = block_on(build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir));
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
