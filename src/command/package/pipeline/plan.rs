// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline plan` — compute which versions need work without
//! side-effects. Used by the GHA `discover` job.

mod drift;
mod env;

// Glob, as elsewhere in this refactor: the `#[path]` test modules resolve
// through `use super::super::*;`. `pub(crate)` because `patch` and
// `prepare` reach several of these by their `plan::` path, which the split
// is not meant to change.
pub(crate) use drift::*;
pub(crate) use env::*;

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::Utc;
use futures::stream::{self, StreamExt, TryStreamExt};
use ocx_lib::cli::DataInterface;
use ocx_lib::log;
use ocx_lib::oci::{Algorithm, Architecture, Identifier, OperatingSystem, Platform};
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
    /// Normalized tag the pipeline publishes — including the
    /// `build_timestamp` stamp, on every source type. Archive sources may
    /// additionally carry a variant prefix (`slim-3.29.0_20260808`); env
    /// sources never do (libc is a platform `os.features` axis there, not a
    /// tag prefix). The whole prepare → test → push chain keys off this
    /// string, so each variant must carry its own tag here.
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
        // Propagated, not re-wrapped: `load_spec` already classifies its own
        // failures — 79 for a missing spec, 65 for a malformed one, 64 for a
        // policy refusal such as a `sign:` block naming a scrubbed variable —
        // and every sibling pipeline command `?`s it straight through. Folding
        // all three into `SourceError` (69) here made `plan` the one step where
        // a refusal reported the wrong exit code and lost its cause chain.
        let spec = spec::load_spec(spec_path).await?;
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
    let client = crate::command::package::registry_client()?;
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
            let versions = build_pylock_plan_entries(
                spec,
                spec_dir,
                path,
                &upstream_versions,
                &all_tags,
                &version_map,
                &build_ts,
            )
            .await?;
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
                build_pypi_plan_entries(spec, &upstream_versions, &all_tags, &version_map, locks_dir, &build_ts)
                    .await?;
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
