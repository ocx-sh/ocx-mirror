// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

pub mod asset_resolution;

use std::collections::{HashMap, HashSet};

use ocx_oci::Platform;
use regex::Regex;

use asset_resolution::{AmbiguousAsset, AssetResolution, ResolvedPlatformAsset};

use crate::source::VersionInfo;

/// Resolve assets for each platform using the configured regex patterns.
///
/// Takes the whole [`VersionInfo`] rather than its asset map: the
/// publisher-declared digest is keyed by the same asset name the regex picked,
/// and stamping it here is the only place both are in scope.
///
/// For each platform, all patterns are applied against all asset names.
/// - 0 matches → platform absent (skipped, not an error)
/// - 1 distinct asset → resolved
/// - 2+ distinct assets → ambiguous (error)
pub fn resolve_assets(version: &VersionInfo, patterns: &HashMap<Platform, Vec<Regex>>) -> AssetResolution {
    let mut resolved = Vec::new();
    let mut ambiguous = Vec::new();

    for (platform, regexes) in patterns {
        let mut matched: HashSet<String> = HashSet::new();

        for regex in regexes {
            for asset_name in version.assets.keys() {
                if regex.is_match(asset_name) {
                    matched.insert(asset_name.clone());
                }
            }
        }

        match matched.len() {
            0 => {} // Platform absent for this version — skip silently
            1 => {
                let asset_name = matched.into_iter().next().expect("len checked above");
                let url = version.assets[&asset_name].clone();
                let digest = version.asset_digests.get(&asset_name).cloned();
                resolved.push(ResolvedPlatformAsset {
                    platform: platform.clone(),
                    asset_name,
                    url,
                    digest,
                });
            }
            _ => {
                ambiguous.push(AmbiguousAsset {
                    platform: platform.clone(),
                    matched_assets: matched.into_iter().collect(),
                });
            }
        }
    }

    if ambiguous.is_empty() {
        AssetResolution::Resolved(resolved)
    } else {
        AssetResolution::Ambiguous(ambiguous)
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn re(s: &str) -> Regex {
        Regex::new(s).unwrap()
    }

    fn platform(s: &str) -> Platform {
        s.parse().unwrap()
    }

    /// A version carrying just the asset map — no source here declares a
    /// digest, so every resolution below stamps `None`.
    fn info(assets: HashMap<String, Url>) -> VersionInfo {
        VersionInfo {
            version: "1.0.0".to_string(),
            assets,
            asset_digests: HashMap::new(),
            is_prerelease: false,
        }
    }

    #[test]
    fn single_pattern_single_match() {
        let mut assets = HashMap::new();
        assets.insert(
            "tool-linux-amd64.tar.gz".to_string(),
            url("https://example.com/tool-linux-amd64.tar.gz"),
        );
        assets.insert(
            "tool-darwin-arm64.tar.gz".to_string(),
            url("https://example.com/tool-darwin-arm64.tar.gz"),
        );

        let mut patterns = HashMap::new();
        patterns.insert(platform("linux/amd64"), vec![re(r"tool-linux-amd64\.tar\.gz")]);
        patterns.insert(platform("darwin/arm64"), vec![re(r"tool-darwin-arm64\.tar\.gz")]);

        match resolve_assets(&info(assets), &patterns) {
            AssetResolution::Resolved(resolved) => {
                assert_eq!(resolved.len(), 2);
                let linux = resolved.iter().find(|r| r.platform == platform("linux/amd64")).unwrap();
                assert_eq!(linux.asset_name, "tool-linux-amd64.tar.gz");
                let darwin = resolved
                    .iter()
                    .find(|r| r.platform == platform("darwin/arm64"))
                    .unwrap();
                assert_eq!(darwin.asset_name, "tool-darwin-arm64.tar.gz");
            }
            AssetResolution::Ambiguous(_) => panic!("Expected resolved"),
        }
    }

    #[test]
    fn multiple_patterns_same_asset_deduplicates() {
        let mut assets = HashMap::new();
        assets.insert(
            "tool-linux-x86_64.tar.gz".to_string(),
            url("https://example.com/tool.tar.gz"),
        );

        let mut patterns = HashMap::new();
        patterns.insert(
            platform("linux/amd64"),
            vec![re(r"tool-linux-x86_64\.tar\.gz"), re(r"tool-linux-.*\.tar\.gz")],
        );

        match resolve_assets(&info(assets), &patterns) {
            AssetResolution::Resolved(resolved) => {
                assert_eq!(resolved.len(), 1);
                assert_eq!(resolved[0].asset_name, "tool-linux-x86_64.tar.gz");
            }
            AssetResolution::Ambiguous(_) => panic!("Expected resolved"),
        }
    }

    #[test]
    fn multiple_patterns_different_assets_ambiguous() {
        let mut assets = HashMap::new();
        assets.insert(
            "tool-linux-amd64.tar.gz".to_string(),
            url("https://example.com/a.tar.gz"),
        );
        assets.insert(
            "tool-linux-x86_64.tar.gz".to_string(),
            url("https://example.com/b.tar.gz"),
        );

        let mut patterns = HashMap::new();
        patterns.insert(
            platform("linux/amd64"),
            vec![re(r"tool-linux-amd64\.tar\.gz"), re(r"tool-linux-x86_64\.tar\.gz")],
        );

        match resolve_assets(&info(assets), &patterns) {
            AssetResolution::Resolved(_) => panic!("Expected ambiguous"),
            AssetResolution::Ambiguous(amb) => {
                assert_eq!(amb.len(), 1);
                assert_eq!(amb[0].platform, platform("linux/amd64"));
                assert_eq!(amb[0].matched_assets.len(), 2);
            }
        }
    }

    #[test]
    fn libc_variants_resolve_independently() {
        // Two libc variants share os/arch but are distinct Platform keys, each
        // with its own pattern list — they resolve to their own asset with no
        // ambiguity.
        let mut assets = HashMap::new();
        assets.insert(
            "cpython-3.12-gnu.tar.zst".to_string(),
            url("https://example.com/cpython-3.12-gnu.tar.zst"),
        );
        assets.insert(
            "cpython-3.12-musl.tar.zst".to_string(),
            url("https://example.com/cpython-3.12-musl.tar.zst"),
        );

        let mut patterns = HashMap::new();
        patterns.insert(
            platform("linux/amd64+libc.glibc"),
            vec![re(r"cpython-.*-gnu\.tar\.zst")],
        );
        patterns.insert(
            platform("linux/amd64+libc.musl"),
            vec![re(r"cpython-.*-musl\.tar\.zst")],
        );

        match resolve_assets(&info(assets), &patterns) {
            AssetResolution::Resolved(resolved) => {
                assert_eq!(resolved.len(), 2);
                let glibc = resolved
                    .iter()
                    .find(|r| r.platform == platform("linux/amd64+libc.glibc"))
                    .unwrap();
                assert_eq!(glibc.asset_name, "cpython-3.12-gnu.tar.zst");
                let musl = resolved
                    .iter()
                    .find(|r| r.platform == platform("linux/amd64+libc.musl"))
                    .unwrap();
                assert_eq!(musl.asset_name, "cpython-3.12-musl.tar.zst");
            }
            AssetResolution::Ambiguous(_) => panic!("Expected resolved"),
        }
    }

    #[test]
    fn zero_matches_platform_absent() {
        let mut assets = HashMap::new();
        assets.insert(
            "tool-linux-amd64.tar.gz".to_string(),
            url("https://example.com/tool.tar.gz"),
        );

        let mut patterns = HashMap::new();
        patterns.insert(platform("linux/amd64"), vec![re(r"tool-linux-amd64\.tar\.gz")]);
        patterns.insert(platform("darwin/arm64"), vec![re(r"tool-darwin-arm64\.tar\.gz")]);

        match resolve_assets(&info(assets), &patterns) {
            AssetResolution::Resolved(resolved) => {
                assert_eq!(resolved.len(), 1);
                assert_eq!(resolved[0].platform, platform("linux/amd64"));
            }
            AssetResolution::Ambiguous(_) => panic!("Expected resolved"),
        }
    }

    #[test]
    fn universal_binary_same_url_different_platforms() {
        let mut assets = HashMap::new();
        assets.insert(
            "tool-macos-universal.tar.gz".to_string(),
            url("https://example.com/tool-macos-universal.tar.gz"),
        );

        let mut patterns = HashMap::new();
        patterns.insert(platform("darwin/amd64"), vec![re(r"tool-macos-universal\.tar\.gz")]);
        patterns.insert(platform("darwin/arm64"), vec![re(r"tool-macos-universal\.tar\.gz")]);

        match resolve_assets(&info(assets), &patterns) {
            AssetResolution::Resolved(resolved) => {
                assert_eq!(resolved.len(), 2);
                // Both platforms resolve to the same asset
                for r in &resolved {
                    assert_eq!(r.asset_name, "tool-macos-universal.tar.gz");
                }
            }
            AssetResolution::Ambiguous(_) => panic!("Expected resolved"),
        }
    }

    #[test]
    fn a_declared_digest_is_stamped_onto_the_resolved_asset() {
        // The carrier: whatever the source declared for the asset name the
        // regex picked travels with the URL. An asset with no declared digest
        // stays `None` — that is every source's state until WP-A fills them.
        let mut assets = HashMap::new();
        assets.insert(
            "tool-linux-amd64.tar.gz".to_string(),
            url("https://example.com/a.tar.gz"),
        );
        assets.insert(
            "tool-darwin-arm64.tar.gz".to_string(),
            url("https://example.com/b.tar.gz"),
        );

        let mut asset_digests = HashMap::new();
        asset_digests.insert("tool-linux-amd64.tar.gz".to_string(), "sha256:abc".to_string());

        let version = VersionInfo {
            version: "1.0.0".to_string(),
            assets,
            asset_digests,
            is_prerelease: false,
        };

        let mut patterns = HashMap::new();
        patterns.insert(platform("linux/amd64"), vec![re(r"tool-linux-amd64\.tar\.gz")]);
        patterns.insert(platform("darwin/arm64"), vec![re(r"tool-darwin-arm64\.tar\.gz")]);

        match resolve_assets(&version, &patterns) {
            AssetResolution::Resolved(resolved) => {
                let linux = resolved.iter().find(|r| r.platform == platform("linux/amd64")).unwrap();
                assert_eq!(linux.digest.as_deref(), Some("sha256:abc"));
                let darwin = resolved
                    .iter()
                    .find(|r| r.platform == platform("darwin/arm64"))
                    .unwrap();
                assert_eq!(darwin.digest, None);
            }
            AssetResolution::Ambiguous(_) => panic!("Expected resolved"),
        }
    }

    #[test]
    fn cmake_naming_convention_old_and_new() {
        let mut assets = HashMap::new();
        assets.insert(
            "cmake-3.28.0-linux-x86_64.tar.gz".to_string(),
            url("https://example.com/cmake-3.28.0-linux-x86_64.tar.gz"),
        );

        let mut patterns = HashMap::new();
        patterns.insert(
            platform("linux/amd64"),
            vec![
                re(r"cmake-.*-linux-x86_64\.tar\.gz"),
                re(r"cmake-.*-Linux-x86_64\.tar\.gz"),
            ],
        );

        match resolve_assets(&info(assets), &patterns) {
            AssetResolution::Resolved(resolved) => {
                assert_eq!(resolved.len(), 1);
                assert_eq!(resolved[0].asset_name, "cmake-3.28.0-linux-x86_64.tar.gz");
            }
            AssetResolution::Ambiguous(_) => panic!("Expected resolved"),
        }
    }
}
