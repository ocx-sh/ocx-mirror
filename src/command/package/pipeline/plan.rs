// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline plan` — compute which versions need work without
//! side-effects. Used by the GHA `discover` job.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use futures::stream::{self, StreamExt, TryStreamExt};
use ocx_lib::cli::DataInterface;
use ocx_lib::log;
use ocx_lib::oci::{Algorithm, ClientBuilder, Identifier, Platform};
use ocx_lib::package::metadata::Metadata;
use ocx_lib::package::version::Version;
use ocx_lib::publisher::Publisher;
use serde::{Deserialize, Serialize};

use crate::command::package::options::OutputFormat;
use crate::command::package::sync::list_upstream_versions;
use crate::command::package::target_registry::{self, PublishedImage};
use crate::error::MirrorError;
use crate::filter;
use crate::normalizer;
use crate::pipeline::orchestrator::{self, ExpectedMetadata, MetadataPlan};
use crate::resolver;
use crate::resolver::asset_resolution::AssetResolution;
use crate::spec::{self, BinScanMode, MirrorSpec};
use crate::version_platform_map::VersionPlatformMap;

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
    /// Variant-prefixed normalized tag the pipeline publishes (e.g. `3.29.0`
    /// for the default variant, `slim-3.29.0` for the `slim` variant). The
    /// whole prepare → test → push chain keys off this string, so each variant
    /// must carry its own tag here.
    pub version: String,
    /// Platform slugs that require work (e.g. `["linux/amd64", "darwin/arm64"]`).
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
}

impl PlanCmd {
    pub async fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        let spec_path = &self.spec;
        let spec = spec::load_spec(spec_path)
            .await
            .map_err(|e| MirrorError::SourceError(format!("failed to load spec: {e}")))?;
        let spec_dir = spec_path.parent().unwrap_or(std::path::Path::new("."));

        let report = build_plan_report(&spec, spec_dir).await?;

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
async fn build_plan_report(spec: &MirrorSpec, spec_dir: &std::path::Path) -> Result<PlanReport, MirrorError> {
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

    // Fetch upstream versions — propagate failures as Unavailable.
    let upstream_versions = list_upstream_versions(spec, spec_dir)
        .await
        .map_err(|e| MirrorError::SourceError(format!("failed to list upstream versions: {e}")))?;

    // Build timestamp (reuse existing normalizer).
    let build_ts = normalizer::build_timestamp(&spec.build_timestamp);

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
/// Never under a `bin_scan`: the expectation is missing the `binaries` claim
/// the registry records, so its digest cannot match a correctly published tile
/// and a digest comparison would report drift on every one of them. Those tiles
/// go straight to the blob read that adopts the published claim.
fn settled_by_digest(image: &PublishedImage, expected: &Metadata, bin_scan: BinScanMode) -> Result<bool, MirrorError> {
    Ok(!bin_scan.scans() && config_bytes_match(image, expected)?)
}

/// Whether the config blob the registry recorded is byte-identical to the one a
/// fresh publish would write, decided from the descriptor's digest alone.
///
/// `true` proves there is no drift, with no blob fetch. `false` proves nothing:
/// the published bytes are whatever the `ocx` that wrote them serialized, so a
/// key-order change alone flips this while the value is unchanged. The caller
/// must fall back to [`metadata_drifted`] rather than report drift here.
fn config_bytes_match(image: &PublishedImage, expected: &Metadata) -> Result<bool, MirrorError> {
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
    fn a_bin_scan_tile_is_never_settled_by_its_config_digest() {
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
                "{mode:?} must fall through to the blob read that adopts the published binaries, \
                 even on an identical config digest",
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
}
