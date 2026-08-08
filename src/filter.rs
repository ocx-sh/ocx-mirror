// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::cmp::Ordering;

use ocx_lib::package::version::Version;

use crate::resolver::asset_resolution::ResolvedPlatformAsset;
use crate::spec::{BackfillOrder, VersionsConfig};
use crate::version_platform_map::VersionPlatformMap;

/// A version with its resolved platform assets, ready for filtering.
#[derive(Debug, Clone)]
pub struct ResolvedVersion {
    pub version: String,
    pub normalized_version: String,
    /// Variant name for this resolution, used for variant-aware already-mirrored checks.
    pub variant: Option<String>,
    pub platforms: Vec<ResolvedPlatformAsset>,
    pub is_prerelease: bool,
}

/// Apply the full filter pipeline to a list of resolved versions.
///
/// Filters applied in order:
/// 1. Exact version match (if `exact_version` is set)
/// 2. Skip prereleases (if `skip_prereleases` is true)
/// 3. Apply min/max version bounds
/// 4. Skip versions with no resolved platform assets
/// 5. Sort by version
/// 6. Keep only the latest (highest) version (if `latest` is true)
/// 7. Subtract already-mirrored versions
/// 8. Apply `new_per_run` cap respecting `backfill` order:
///    - `newest_first` (default): newest non-mirrored versions first
///    - `oldest_first`: chronological backfill from the oldest
///
/// Note: `latest` is applied before subtracting already-mirrored versions so that
/// `--latest` always targets the true latest version. If it's already mirrored,
/// the result is empty rather than falling back to the next-highest version.
pub fn filter_versions(
    mut versions: Vec<ResolvedVersion>,
    exact_versions: &[String],
    skip_prereleases: bool,
    versions_config: Option<&VersionsConfig>,
    existing: &VersionPlatformMap,
    latest: bool,
) -> Vec<ResolvedVersion> {
    // 1. Exact version match (version-aware: "3.12.13+20260310" matches "3.12.13_20260310")
    if !exact_versions.is_empty() {
        let parsed_exact: Vec<Option<Version>> = exact_versions.iter().map(|s| Version::parse(s)).collect();
        versions.retain(|v| {
            // Try parsed comparison first (handles +/_ equivalence), fall back to raw string
            let v_parsed = Version::parse(&v.version);
            exact_versions.iter().zip(&parsed_exact).any(|(raw, parsed)| {
                if let (Some(vp), Some(ep)) = (&v_parsed, parsed) {
                    vp == ep
                } else {
                    v.version == *raw
                }
            })
        });
    }

    // 2. Skip prereleases
    if skip_prereleases {
        versions.retain(|v| !v.is_prerelease);
    }

    // 2. Apply min/max bounds
    if let Some(config) = versions_config {
        versions.retain(|v| within_bounds(&v.version, config.min.as_deref(), config.max.as_deref()));
    }

    // 3. Skip versions with no resolved platform assets
    versions.retain(|v| !v.platforms.is_empty());

    // 4. Sort by version (oldest first)
    versions.sort_by(|a, b| {
        let va = Version::parse(&a.version);
        let vb = Version::parse(&b.version);
        match (va, vb) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => a.version.cmp(&b.version),
        }
    });

    // 5. Keep only the latest (highest) version BEFORE subtracting already-mirrored.
    // This ensures --latest always targets the true latest, and if it's already
    // mirrored, the result is empty (nothing to do) rather than falling back
    // to the next-highest version.
    if latest && let Some(last) = versions.last() {
        let latest_version = last.version.clone();
        versions.retain(|v| v.version == latest_version);
    }

    // 6. Subtract already-mirrored (version, platform) pairs.
    // A version is kept if at least one of its target platforms is not yet pushed.
    // This enables retry of partially-pushed versions (e.g., linux/amd64 succeeded
    // but darwin/arm64 failed on a previous run).
    //
    // For variant specs, the check uses the variant-prefixed version (e.g.,
    // "debug-3.12.5") to match registry tags. Min/max bounds (step 2) use the
    // bare source version to avoid Ord issues with variant-first sorting.
    versions.retain_mut(|v| {
        let check_tag = match &v.variant {
            Some(name) => format!("{name}-{}", v.version),
            None => v.version.clone(),
        };
        let version = Version::parse(&check_tag).expect("mirror versions must be valid");
        v.platforms.retain(|pa| !existing.has(&version, &pa.platform));
        !v.platforms.is_empty()
    });

    // 7. Apply new_per_run cap (not applicable when --latest is set)
    if !latest
        && let Some(config) = versions_config
        && let Some(cap) = config.new_per_run
    {
        match config.backfill {
            BackfillOrder::OldestFirst => {
                versions.truncate(cap);
            }
            BackfillOrder::NewestFirst => {
                let start = versions.len().saturating_sub(cap);
                versions = versions.split_off(start);
            }
        }
    }

    versions
}

/// Order a version string against a declared bound (`versions.min`/`max`, a
/// platform's `min_version`/`max_version`, an `exclude` range or an `exclude`
/// single version).
///
/// `ocx_lib::Version` first: every bound is validated against that parser
/// (`VersionsConfig::validate`, `validate_platforms`), and it disagrees with
/// PEP 440 on strings both accept — `1.0.0+build1 < 1.0.0` here versus `>`
/// there (build metadata versus a PEP 440 local version), and `1.2 > 1.2.0`
/// here (a rolling parent outranks its own leaf) versus `==` there. Asking it
/// first keeps every semver mirror comparing exactly as it does today.
///
/// PEP 440 second, for the tags that parser rejects: it caps at three
/// components, so real upstream Python releases (`0.16.2.0`, `2.0.0.dev0`)
/// parsed as nothing at all and — under the fail-open convention below —
/// satisfied every bound they were measured against.
///
/// Not [`pep440_sort_key`]: that is a sort key, not a comparator. Its `None`
/// arm sorts first, so a tag neither parser understands would fall below every
/// `min` and be dropped rather than kept; and its text tiebreak separates
/// PEP 440-equal versions (`1.16.6` vs `1.16.6.0`), which would push a
/// candidate off an inclusive `min` boundary it sits exactly on.
///
/// `None` means no comparator related both sides — the caller leaves the
/// version unbounded rather than guessing.
pub(crate) fn version_cmp(candidate: &str, bound: &str) -> Option<Ordering> {
    if let (Some(candidate), Some(bound)) = (Version::parse(candidate), Version::parse(bound)) {
        return Some(candidate.cmp(&bound));
    }
    let candidate: ocx_python::uv_pep440::Version = candidate.parse().ok()?;
    let bound: ocx_python::uv_pep440::Version = bound.parse().ok()?;
    Some(candidate.cmp(&bound))
}

/// Whether `candidate` falls in the half-open window `[min, max)` — the
/// min-inclusive / max-exclusive convention shared by `versions:`, per-platform
/// `min_version`/`max_version`, and `exclude:` ranges.
///
/// Fail-open: a bound [`version_cmp`] cannot relate to `candidate` does not
/// constrain it, so an unrecognisable upstream tag is surfaced as work rather
/// than silently skipped.
pub(crate) fn within_bounds(candidate: &str, min: Option<&str>, max: Option<&str>) -> bool {
    if let Some(min) = min
        && version_cmp(candidate, min) == Some(Ordering::Less)
    {
        return false;
    }
    if let Some(max) = max
        && matches!(version_cmp(candidate, max), Some(Ordering::Greater | Ordering::Equal))
    {
        return false;
    }
    true
}

/// Total-order sort key for a PEP 440 version string:
/// `(parsed version, original text)`.
///
/// `None` sorts before `Some`, so an unparseable tag lands first and the
/// newest parseable version is LAST — which is what every "newest = last
/// element" reader here relies on. The text tiebreaks equal parses so the key
/// is a total order on distinct strings.
///
/// It replaces a pairwise comparator of the shape
/// `match (parse(a), parse(b)) { (Some, Some) => semver, _ => text }`, which is
/// not transitive and therefore not a valid `sort_by` predicate: with
/// `"10.0.0"`, `"3.0.0"` and `"2.0rc1"` (the last unparseable by
/// `ocx_lib::Version`) it yields `10.0.0 > 3.0.0 > 2.0rc1 > 10.0.0` — a cycle,
/// for which `slice::sort_by` documents an unspecified order and permits a
/// panic. Here that order decides push order and which version `:latest`
/// lands on.
///
/// `uv_pep440` rather than `ocx_lib::package::version::Version`: upstream
/// Python versions are PEP 440 (`0.0.0.2`, `2.0.0.dev0`), which the ≤3-component
/// OCX parser rejects. The `Version::parse` check that decides `--cascade`
/// stays as it is — that one asks a different question ("can ocx derive
/// rolling tags from this?").
///
/// A published tag may carry the mirror's build stamp, which OCX spells with
/// `_` (`1.16.6_20260808`) where PEP 440 spells a local version with `+`. That
/// form parses as nothing, and the text fallback then orders `1.16.6_…` BEFORE
/// `1.9.0_…` and ranks any bare tag above every stamped one — so the env push
/// loop, which sorts its manifests by this key, would push out of order and
/// land `latest` on the wrong version. The separator is therefore retried as
/// `+` when, and only when, the tag does not parse as it stands: a `_` PEP 440
/// itself gives a meaning (`1.0_alpha1` → `1.0a1`) parses on the first attempt
/// and never reaches the retry.
///
/// The retry rewrites the LAST `_`, not the first: the stamp is always the
/// trailing component, and a release may itself carry earlier ones
/// (`1.0_alpha1_20260808`). Rewriting the first would read the prerelease as
/// part of a local version (`1.0+alpha1.20260808`), which sorts ABOVE its own
/// release instead of below it.
pub(crate) fn pep440_sort_key(version: &str) -> (Option<ocx_python::uv_pep440::Version>, String) {
    let parsed = version.parse().ok().or_else(|| {
        version
            .rsplit_once('_')
            .and_then(|(release, stamp)| format!("{release}+{stamp}").parse().ok())
    });
    (parsed, version.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn pep440_sort_key_is_a_total_order_over_versions_the_ocx_parser_rejects() {
        // The replaced comparator was `(Some, Some) => semver, _ => text`, which
        // on this exact triple cycles: `ocx_lib::Version` rejects `2.0rc1`, so
        // 10.0.0 > 3.0.0 (semver), 3.0.0 > 2.0rc1 (text) and 2.0rc1 > 10.0.0
        // (text). `sort_by` leaves the result unspecified for such a predicate.
        let mut versions = vec![
            "10.0.0".to_string(),
            "3.0.0".to_string(),
            "2.0rc1".to_string(),
            "2.0".to_string(),
            "0.0.0.2".to_string(),
        ];
        versions.sort_by_key(|v| pep440_sort_key(v));
        assert_eq!(versions, ["0.0.0.2", "2.0rc1", "2.0", "3.0.0", "10.0.0"]);

        // Newest LAST is the contract every `.last()` reader here depends on.
        assert_eq!(versions.last().map(String::as_str), Some("10.0.0"));

        // A tag no PEP 440 parser accepts sorts first, so it can never be
        // mistaken for the newest.
        let mut mixed = vec!["nightly".to_string(), "1.0.0".to_string()];
        mixed.sort_by_key(|v| pep440_sort_key(v));
        assert_eq!(mixed, ["nightly", "1.0.0"]);
    }

    #[test]
    fn pep440_sort_key_orders_build_stamped_tags_by_release() {
        // `pipeline push` sorts an env run's versions by this key, and since
        // the env plan stamps its tags every one of them carries `_<ts>` —
        // which PEP 440 spells `+<local>`, so the raw form parses as nothing
        // and the key degrades to TEXT order: `1.16.6_…` would sort BEFORE
        // `1.9.0_…` and a bare tag would outrank every stamped one, pushing
        // the versions out of order and landing `latest` on the wrong one.
        let mut versions = vec![
            "1.16.6_20260808".to_string(),
            "1.9.0_20260808".to_string(),
            "1.16.6".to_string(),
        ];
        versions.sort_by_key(|v| pep440_sort_key(v));
        assert_eq!(versions, ["1.9.0_20260808", "1.16.6", "1.16.6_20260808"]);

        // A `_` that PEP 440 itself gives a meaning keeps it: `1.0_alpha1`
        // parses as-is (→ `1.0a1`), so it never reaches the build-stamp
        // reading and must not be read as a local version.
        let mut prereleases = vec!["1.0".to_string(), "1.0_alpha1".to_string()];
        prereleases.sort_by_key(|v| pep440_sort_key(v));
        assert_eq!(prereleases, ["1.0_alpha1", "1.0"], "alpha sorts before the release");

        // Both at once: a prerelease spelled with `_` AND a build stamp. Only
        // the LAST `_` is the stamp. Rewriting the FIRST one instead yields
        // `1.0+alpha1.20260808` — a local version of the release, which PEP 440
        // ranks ABOVE `1.0`, so the alpha would be pushed after its own release
        // and could take `latest` off it.
        let mut stamped_prerelease = vec![
            "1.0".to_string(),
            "1.0_20260808".to_string(),
            "1.0_alpha1_20260808".to_string(),
        ];
        stamped_prerelease.sort_by_key(|v| pep440_sort_key(v));
        assert_eq!(
            stamped_prerelease,
            ["1.0_alpha1_20260808", "1.0", "1.0_20260808"],
            "the stamped alpha sorts before the release; the stamped release after it"
        );
    }

    use ocx_lib::oci::Platform;
    use url::Url;

    use super::*;

    #[test]
    fn version_cmp_asks_the_ocx_parser_first_then_pep440() {
        // Both parsers accept these and order them differently: build metadata
        // sorts BELOW its release here but a PEP 440 local sorts above it, and a
        // rolling parent outranks its own leaf here but is equal there. The ocx
        // answer is the one every semver mirror already filters by.
        assert_eq!(version_cmp("1.0.0+build1", "1.0.0"), Some(Ordering::Less));
        assert_eq!(version_cmp("1.2", "1.2.0"), Some(Ordering::Greater));

        // Beyond that parser's three components, PEP 440 decides.
        assert_eq!(version_cmp("0.16.2.0", "1.16.0"), Some(Ordering::Less));
        assert_eq!(version_cmp("1.16.6.0", "1.16.6"), Some(Ordering::Equal));
        assert_eq!(version_cmp("1.16.0rc1", "1.16.0"), Some(Ordering::Less));
    }

    #[test]
    fn within_bounds_leaves_a_tag_neither_parser_understands_unbounded() {
        // Fail-open: `nightly` is not an ocx version and not PEP 440, so it stays
        // visible as work instead of being silently dropped under a floor.
        assert!(version_cmp("nightly", "1.0.0").is_none());
        assert!(within_bounds("nightly", Some("1.0.0"), Some("2.0.0")));
        assert!(within_bounds("1.5.0", None, None));
    }

    fn platform(s: &str) -> Platform {
        s.parse().unwrap()
    }

    fn rv(version: &str, normalized: &str, prerelease: bool) -> ResolvedVersion {
        ResolvedVersion {
            version: version.to_string(),
            normalized_version: normalized.to_string(),
            variant: None,
            platforms: vec![ResolvedPlatformAsset {
                platform: platform("linux/amd64"),
                asset_name: "test.tar.gz".to_string(),
                url: Url::parse("https://example.com/test.tar.gz").unwrap(),
            }],
            is_prerelease: prerelease,
        }
    }

    fn rv_variant(version: &str, normalized: &str, variant: &str) -> ResolvedVersion {
        ResolvedVersion {
            version: version.to_string(),
            normalized_version: normalized.to_string(),
            variant: Some(variant.to_string()),
            platforms: vec![ResolvedPlatformAsset {
                platform: platform("linux/amd64"),
                asset_name: "test.tar.gz".to_string(),
                url: Url::parse("https://example.com/test.tar.gz").unwrap(),
            }],
            is_prerelease: false,
        }
    }

    fn rv_multi(version: &str, normalized: &str, platforms: &[&str]) -> ResolvedVersion {
        ResolvedVersion {
            version: version.to_string(),
            normalized_version: normalized.to_string(),
            variant: None,
            platforms: platforms
                .iter()
                .map(|p| ResolvedPlatformAsset {
                    platform: platform(p),
                    asset_name: "test.tar.gz".to_string(),
                    url: Url::parse("https://example.com/test.tar.gz").unwrap(),
                })
                .collect(),
            is_prerelease: false,
        }
    }

    /// Build a VersionPlatformMap with the given (version, platform) pairs already pushed.
    fn existing(pairs: &[(&str, &str)]) -> VersionPlatformMap {
        let mut map = VersionPlatformMap::default();
        for (v, p) in pairs {
            map.add(Version::parse(v).unwrap(), platform(p));
        }
        map
    }

    fn empty() -> VersionPlatformMap {
        VersionPlatformMap::default()
    }

    #[test]
    fn skip_prereleases_when_configured() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("1.1.0-rc1", "1.1.0-rc1+ts", true),
            rv("2.0.0", "2.0.0+ts", false),
        ];

        let result = filter_versions(versions, &[], true, None, &empty(), false);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|v| !v.is_prerelease));
    }

    #[test]
    fn keep_prereleases_when_not_configured() {
        let versions = vec![rv("1.0.0", "1.0.0+ts", false), rv("1.1.0-rc1", "1.1.0-rc1+ts", true)];

        let result = filter_versions(versions, &[], false, None, &empty(), false);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_min_bound() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let config = VersionsConfig {
            min: Some("2.0.0".to_string()),
            ..Default::default()
        };

        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].version, "2.0.0");
        assert_eq!(result[1].version, "3.0.0");
    }

    #[test]
    fn filter_max_bound_exclusive() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let config = VersionsConfig {
            max: Some("2.0.0".to_string()),
            ..Default::default()
        };

        // max is exclusive: 2.0.0 itself is excluded
        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "1.0.0");
    }

    #[test]
    fn min_bound_drops_four_segment_pep440_versions() {
        // Live regression (pipx, `min: "1.16.0"`): `ocx_lib::Version` rejects a
        // 4-segment PEP 440 release, and the bounds filter kept every version it
        // could not parse — so seven sub-1.0 releases planned as new work.
        let versions = vec![
            rv("0.15.5.1", "0.15.5.1+ts", false),
            rv("0.16.2.0", "0.16.2.0+ts", false),
            rv("1.16.6", "1.16.6+ts", false),
        ];

        let config = VersionsConfig {
            min: Some("1.16.0".to_string()),
            ..Default::default()
        };

        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        let kept: Vec<&str> = result.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(kept, ["1.16.6"]);
    }

    #[test]
    fn max_bound_drops_four_segment_pep440_versions() {
        // `1.0.0.0` is PEP 440-equal to `1.0.0`, and max is exclusive.
        let versions = vec![
            rv("0.9.0", "0.9.0+ts", false),
            rv("1.0.0.0", "1.0.0.0+ts", false),
            rv("1.2.3.4", "1.2.3.4+ts", false),
        ];

        let config = VersionsConfig {
            max: Some("1.0.0".to_string()),
            ..Default::default()
        };

        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        let kept: Vec<&str> = result.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(kept, ["0.9.0"]);
    }

    #[test]
    fn subtract_already_mirrored() {
        let versions = vec![
            rv("1.0.0", "1.0.0_20260313150000", false),
            rv("2.0.0", "2.0.0_20260313150000", false),
            rv("3.0.0", "3.0.0_20260313150000", false),
        ];

        // 1.0.0 and 3.0.0 already pushed for linux/amd64
        let existing = existing(&[("1.0.0", "linux/amd64"), ("3.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
    }

    #[test]
    fn subtract_already_mirrored_with_build_metadata() {
        let versions = vec![
            rv("1.0.0+build1", "1.0.0_build1_20260313150000", false),
            rv("2.0.0", "2.0.0_20260313150000", false),
        ];

        // "1.0.0+build1" normalizes to "1.0.0_build1" — already pushed
        let existing = existing(&[("1.0.0_build1", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
    }

    #[test]
    fn new_per_run_cap_newest_first() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let config = VersionsConfig {
            new_per_run: Some(1),
            ..Default::default()
        };

        // Default (newest_first): picks the highest version
        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "3.0.0");
    }

    #[test]
    fn new_per_run_cap_oldest_first() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let config = VersionsConfig {
            new_per_run: Some(1),
            backfill: BackfillOrder::OldestFirst,
            ..Default::default()
        };

        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "1.0.0");
    }

    #[test]
    fn newest_first_with_larger_cap() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
            rv("4.0.0", "4.0.0+ts", false),
            rv("5.0.0", "5.0.0+ts", false),
        ];

        let config = VersionsConfig {
            new_per_run: Some(3),
            ..Default::default()
        };

        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].version, "3.0.0");
        assert_eq!(result[1].version, "4.0.0");
        assert_eq!(result[2].version, "5.0.0");
    }

    #[test]
    fn newest_first_successive_runs() {
        // Simulates day 1: get newest 2, day 2: get next newest 2
        let all_versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
            rv("4.0.0", "4.0.0+ts", false),
        ];

        let config = VersionsConfig {
            new_per_run: Some(2),
            ..Default::default()
        };

        // Day 1: nothing mirrored yet → get [3.0.0, 4.0.0]
        let result = filter_versions(all_versions.clone(), &[], false, Some(&config), &empty(), false);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].version, "3.0.0");
        assert_eq!(result[1].version, "4.0.0");

        // Day 2: 3.0.0 and 4.0.0 already mirrored → get [1.0.0, 2.0.0]
        let existing = existing(&[("3.0.0", "linux/amd64"), ("4.0.0", "linux/amd64")]);
        let result = filter_versions(all_versions, &[], false, Some(&config), &existing, false);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].version, "1.0.0");
        assert_eq!(result[1].version, "2.0.0");
    }

    #[test]
    fn combined_filters() {
        let versions = vec![
            rv("0.9.0", "0.9.0+ts", false),
            rv("1.0.0", "1.0.0+ts", false),
            rv("1.1.0-rc1", "1.1.0-rc1+ts", true),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        // max is exclusive, so 3.0.0 is excluded; prerelease 1.1.0-rc1 also skipped
        let config = VersionsConfig {
            min: Some("1.0.0".to_string()),
            max: Some("3.0.0".to_string()),
            new_per_run: Some(2),
            ..Default::default()
        };

        let existing = existing(&[("1.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], true, Some(&config), &existing, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
    }

    #[test]
    fn skip_versions_with_no_platforms() {
        let mut no_platforms = rv("1.0.0", "1.0.0+ts", false);
        no_platforms.platforms = vec![];

        let versions = vec![no_platforms, rv("2.0.0", "2.0.0+ts", false)];

        let result = filter_versions(versions, &[], false, None, &empty(), false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
    }

    #[test]
    fn subtract_already_mirrored_prerelease() {
        let versions = vec![
            rv("1.0.0-rc1", "1.0.0-rc1_20260313150000", true),
            rv("2.0.0", "2.0.0_20260313150000", false),
        ];

        let existing = existing(&[("1.0.0-rc1", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
    }

    #[test]
    fn exact_version_filter() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let result = filter_versions(versions, &["2.0.0".to_string()], false, None, &empty(), false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
    }

    #[test]
    fn exact_version_plus_underscore_equivalence() {
        // Generator produces "3.12.13+20260310" (with +), user passes --version 3.12.13_20260310 (with _)
        let versions = vec![
            rv("3.12.13+20260310", "3.12.13_20260310", false),
            rv("3.13.0+20260310", "3.13.0_20260310", false),
        ];

        let result = filter_versions(
            versions,
            &["3.12.13_20260310".to_string()],
            false,
            None,
            &empty(),
            false,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "3.12.13+20260310");
    }

    #[test]
    fn exact_version_no_match() {
        let versions = vec![rv("1.0.0", "1.0.0+ts", false), rv("2.0.0", "2.0.0+ts", false)];

        let result = filter_versions(versions, &["9.9.9".to_string()], false, None, &empty(), false);
        assert!(result.is_empty());
    }

    #[test]
    fn exact_version_already_mirrored() {
        let versions = vec![rv("2.0.0", "2.0.0_20260313150000", false)];
        let existing = existing(&[("2.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &["2.0.0".to_string()], false, None, &existing, false);
        assert!(result.is_empty());
    }

    #[test]
    fn multiple_exact_versions() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let result = filter_versions(
            versions,
            &["1.0.0".to_string(), "3.0.0".to_string()],
            false,
            None,
            &empty(),
            false,
        );
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].version, "1.0.0");
        assert_eq!(result[1].version, "3.0.0");
    }

    #[test]
    fn empty_input() {
        let result = filter_versions(vec![], &[], false, None, &empty(), false);
        assert!(result.is_empty());
    }

    #[test]
    fn all_filtered() {
        let versions = vec![rv("1.0.0", "1.0.0_20260313150000", false)];
        let existing = existing(&[("1.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert!(result.is_empty());
    }

    #[test]
    fn partial_platform_retry() {
        // Version has 2 platforms, only 1 is already pushed
        let versions = vec![rv_multi("1.0.0", "1.0.0_ts", &["linux/amd64", "darwin/arm64"])];
        let existing = existing(&[("1.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].platforms.len(), 1);
        assert_eq!(result[0].platforms[0].platform, platform("darwin/arm64"));
    }

    #[test]
    fn all_platforms_pushed_filters_version() {
        let versions = vec![rv_multi("1.0.0", "1.0.0_ts", &["linux/amd64", "darwin/arm64"])];
        let existing = existing(&[("1.0.0", "linux/amd64"), ("1.0.0", "darwin/arm64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert!(result.is_empty());
    }

    #[test]
    fn no_platforms_pushed_keeps_all() {
        let versions = vec![rv_multi("1.0.0", "1.0.0_ts", &["linux/amd64", "darwin/arm64"])];

        let result = filter_versions(versions, &[], false, None, &empty(), false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].platforms.len(), 2);
    }

    #[test]
    fn latest_keeps_highest_version() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let result = filter_versions(versions, &[], false, None, &empty(), true);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "3.0.0");
    }

    #[test]
    fn latest_with_empty_input() {
        let result = filter_versions(vec![], &[], false, None, &empty(), true);
        assert!(result.is_empty());
    }

    #[test]
    fn latest_skips_new_per_run_cap() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
            rv("4.0.0", "4.0.0+ts", false),
            rv("5.0.0", "5.0.0+ts", false),
        ];

        let config = VersionsConfig {
            new_per_run: Some(2),
            ..Default::default()
        };

        // Without --latest, new_per_run=2 keeps [1.0.0, 2.0.0]
        // With --latest, should get 5.0.0 (the true highest), not 2.0.0
        let result = filter_versions(versions, &[], false, Some(&config), &empty(), true);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "5.0.0");
    }

    #[test]
    fn latest_combined_with_exact_versions() {
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let result = filter_versions(
            versions,
            &["1.0.0".to_string(), "2.0.0".to_string()],
            false,
            None,
            &empty(),
            true,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
    }

    #[test]
    fn latest_does_not_fallback_when_already_mirrored() {
        // Regression: --latest should target the true latest (3.0.0), and if it's
        // already mirrored, return empty — NOT fall back to the next-highest (2.0.0).
        let versions = vec![
            rv("1.0.0", "1.0.0+ts", false),
            rv("2.0.0", "2.0.0+ts", false),
            rv("3.0.0", "3.0.0+ts", false),
        ];

        let existing = existing(&[("3.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, true);
        assert!(
            result.is_empty(),
            "should be empty when latest is already mirrored, got: {:?}",
            result.iter().map(|v| &v.version).collect::<Vec<_>>()
        );
    }

    #[test]
    fn latest_retries_partial_platforms() {
        // --latest should still return the latest if only some platforms are mirrored
        let versions = vec![
            rv_multi("1.0.0", "1.0.0_ts", &["linux/amd64", "darwin/arm64"]),
            rv_multi("2.0.0", "2.0.0_ts", &["linux/amd64", "darwin/arm64"]),
        ];

        let existing = existing(&[("2.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, true);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "2.0.0");
        assert_eq!(result[0].platforms.len(), 1);
        assert_eq!(result[0].platforms[0].platform, platform("darwin/arm64"));
    }

    #[test]
    fn version_normalizes_plus_to_underscore() {
        let v = Version::parse("3.15.0+build1").unwrap();
        assert_eq!(v.to_string(), "3.15.0_build1");
    }

    #[test]
    fn version_no_plus_unchanged() {
        let v = Version::parse("3.15.0").unwrap();
        assert_eq!(v.to_string(), "3.15.0");
    }

    #[test]
    fn version_prerelease_with_plus() {
        let v = Version::parse("3.15.0-rc1+build1").unwrap();
        assert_eq!(v.to_string(), "3.15.0-rc1_build1");
    }

    // -- variant-aware filter tests --

    #[test]
    fn variant_already_mirrored_detected() {
        // debug-1.0.0 is already on registry, should be filtered out
        let versions = vec![rv_variant("1.0.0", "debug-1.0.0_ts", "debug")];
        let existing = existing(&[("debug-1.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert!(
            result.is_empty(),
            "variant version should be detected as already mirrored"
        );
    }

    #[test]
    fn variant_not_confused_with_default() {
        // Default variant 1.0.0 is mirrored, but debug-1.0.0 is not
        let versions = vec![rv_variant("1.0.0", "debug-1.0.0_ts", "debug")];
        let existing = existing(&[("1.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert_eq!(result.len(), 1, "debug variant should not be confused with default");
    }

    #[test]
    fn different_variants_same_version_independent() {
        // Both debug and pgo.lto for 1.0.0, only debug is mirrored
        let versions = vec![
            rv_variant("1.0.0", "debug-1.0.0_ts", "debug"),
            rv_variant("1.0.0", "pgo.lto-1.0.0_ts", "pgo.lto"),
        ];
        let existing = existing(&[("debug-1.0.0", "linux/amd64")]);

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].variant.as_deref(), Some("pgo.lto"));
    }

    #[test]
    fn unnamed_default_mirrored_slim_still_uploaded() {
        // Default (unnamed) variant is fully mirrored, slim variant should still pass
        let versions = vec![
            rv("1.0.0", "1.0.0_ts", false),               // default (unnamed)
            rv_variant("1.0.0", "slim-1.0.0_ts", "slim"), // slim
        ];
        let existing = existing(&[("1.0.0", "linux/amd64")]); // only default is on registry

        let result = filter_versions(versions, &[], false, None, &existing, false);
        assert_eq!(result.len(), 1, "slim variant should still be uploaded");
        assert_eq!(result[0].variant.as_deref(), Some("slim"));
    }

    #[test]
    fn variant_min_max_uses_bare_version() {
        // Min/max bounds should apply to the bare source version, not variant-prefixed
        let config = VersionsConfig {
            min: Some("2.0.0".to_string()),
            max: Some("4.0.0".to_string()),
            ..Default::default()
        };

        let versions = vec![
            rv_variant("1.0.0", "debug-1.0.0_ts", "debug"),
            rv_variant("3.0.0", "debug-3.0.0_ts", "debug"),
            rv_variant("5.0.0", "debug-5.0.0_ts", "debug"),
        ];

        let result = filter_versions(versions, &[], false, Some(&config), &empty(), false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "3.0.0");
    }

    #[test]
    fn variant_latest_keeps_all_variants() {
        // --latest should keep ALL variants of the latest source version, not just one
        let versions = vec![
            rv_variant("3.11.0", "debug-3.11.0_ts", "debug"),
            rv_variant("3.11.0", "pgo.lto-3.11.0_ts", "pgo.lto"),
            rv_variant("3.12.5", "debug-3.12.5_ts", "debug"),
            rv_variant("3.12.5", "pgo.lto-3.12.5_ts", "pgo.lto"),
        ];

        let result = filter_versions(versions, &[], false, None, &empty(), true);
        assert_eq!(result.len(), 2, "both variants of 3.12.5 should be kept");
        assert_eq!(result[0].version, "3.12.5");
        assert_eq!(result[0].variant.as_deref(), Some("debug"));
        assert_eq!(result[1].version, "3.12.5");
        assert_eq!(result[1].variant.as_deref(), Some("pgo.lto"));
    }
}
