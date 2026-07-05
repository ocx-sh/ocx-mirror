// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline plan` — compute which versions need work without
//! side-effects. Used by the GHA `discover` job.

use std::collections::HashSet;
use std::path::PathBuf;

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
use crate::resolver;
use crate::resolver::asset_resolution::AssetResolution;
use crate::source;
use crate::spec::{self, MirrorSpec, PythonConfig, Source, VariantSpec};
use crate::version_platform_map::VersionPlatformMap;

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
    if let Source::Pylock { path, .. } = &spec.source {
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
/// each declared platform key that `spec.platform_applies` accepts and is not
/// already published (per `version_map`), resolves a `PythonTarget` per
/// variant and calls `ocx_python::select_wheels` directly, emitting one
/// `PlanAssetEntry` per selected wheel (N per platform — the shape the regex
/// resolver cannot express). Takes an already-parsed `lock`/`app_version` so
/// it never touches the filesystem — network-free and directly unit-testable.
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

    let mut platform_keys: Vec<&str> = spec
        .platforms
        .as_ref()
        .map_or_else(Vec::new, |platforms| platforms.keys().map(String::as_str).collect());
    platform_keys.sort_unstable();
    let declared_platform_count = platform_keys.len();

    let mut entries = Vec::new();
    for (variant_name, constraints) in pylock_variants(spec) {
        let tagged = match variant_name {
            Some(name) => format!("{name}-{app_version}"),
            None => app_version.to_string(),
        };
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
        let check_version = Version::parse(&tagged);

        let mut missing_platforms = Vec::new();
        let mut assets = Vec::new();

        for &platform_key in &platform_keys {
            if !spec.platform_applies(app_version, platform_key) {
                continue;
            }
            let platform: Platform = platform_key
                .parse()
                .map_err(|e| MirrorError::PylockError(format!("invalid platform key '{platform_key}': {e}")))?;
            if check_version
                .as_ref()
                .is_some_and(|version| version_map.has(version, &platform))
            {
                continue; // already published for this (variant, platform)
            }

            let target = PythonTarget {
                platform: pylock_target_platform(&platform, platform_key)?,
                variant: constraints.clone(),
                interpreter: interpreter.clone(),
            };

            let wheels = ocx_python::select_wheels(lock, &target).map_err(|e| {
                MirrorError::PylockError(format!("wheel selection failed for platform '{platform_key}': {e}"))
            })?;

            missing_platforms.push(platform_key.to_string());
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
                    platform: platform_key.to_string(),
                    asset_name: wheel.filename,
                    url,
                });
            }
        }

        if missing_platforms.is_empty() {
            continue; // this variant has no outstanding work for any declared platform
        }

        // Same New/BackfillPartial convention as build_version_entries: the
        // bare (variant-prefixed, un-timestamped) tag already on the registry
        // means some platform was published before, so a shorter missing-set
        // than the declared count is a backfill, not a first publish.
        let version_on_registry = Version::parse(app_version)
            .is_some_and(|v| all_tags.iter().any(|t| Version::parse(t).is_some_and(|tv| tv == v)));
        let kind = if version_on_registry && declared_platform_count > missing_platforms.len() {
            PlanVersionKind::BackfillPartial
        } else {
            PlanVersionKind::New
        };

        entries.push(PlanVersionEntry {
            version: tagged,
            platforms: missing_platforms,
            kind,
            source_version: app_version.to_string(),
            variant: variant_name.map(str::to_string),
            assets,
        });
    }

    Ok(entries)
}

/// Enumerates `pylock` variants directly from `VariantSpec`'s constraint
/// fields — unlike `effective_variants()` (the regex asset-pattern path),
/// which filters pylock variants out (they carry no `assets`). Absent
/// `variants:` is one implicit, unconstrained default variant (matches
/// `select`'s own defaults: gnu libc + manylinux_2_28 floor).
pub(crate) fn pylock_variants(spec: &MirrorSpec) -> Vec<(Option<&str>, VariantConstraints)> {
    match &spec.variants {
        Some(variants) => variants
            .iter()
            .map(|variant| (variant.name.as_deref(), pylock_variant_constraints(variant)))
            .collect(),
        None => vec![(None, VariantConstraints::default())],
    }
}

/// Maps a pylock `VariantSpec`'s constraint fields to `ocx_python`'s
/// `VariantConstraints`. `VariantSpec::validate_python_constraints` already
/// restricts `libc` to `"gnu"`/`"musl"`, so anything else defaults to `Gnu`.
pub(crate) fn pylock_variant_constraints(variant: &VariantSpec) -> VariantConstraints {
    VariantConstraints {
        libc: variant.libc.as_deref().map(|libc| {
            if libc == "musl" {
                LibcFamily::Musl
            } else {
                LibcFamily::Gnu
            }
        }),
        min_manylinux: variant.min_manylinux.clone(),
        min_musllinux: variant.min_musllinux.clone(),
        abi: variant.abi.clone(),
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

/// Maps a spec platform key's parsed `ocx_lib::oci::Platform` to
/// `ocx_python`'s `TargetPlatform` (os/arch only — L2 v1 keeps libc/ABI in
/// the variant prefix, not the platform key).
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
}
