// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use anyhow::{Result, bail};
use chrono::Utc;
use ocx_lib::package::version::Version;

use crate::spec::BuildTimestampFormat;

/// Generate a UTC build timestamp string for the current run.
pub fn build_timestamp(format: &BuildTimestampFormat) -> Option<String> {
    let now = Utc::now();
    match format {
        BuildTimestampFormat::Datetime => Some(now.format("%Y%m%d%H%M%S").to_string()),
        BuildTimestampFormat::Date => Some(now.format("%Y%m%d").to_string()),
        BuildTimestampFormat::None => None,
    }
}

/// Normalize a version string, optionally appending a build timestamp.
///
/// Rules (when build is Some):
/// - `X` → Error (major only, too ambiguous)
/// - `X.Y` → Error (minor only, need full X.Y.Z)
/// - `X.Y.Z` → `X.Y.Z+{build}`
/// - `X.Y.Z-pre` → `X.Y.Z-pre+{build}`
/// - `X.Y.Z+build` → Error (already has build metadata)
///
/// When build is None:
/// - `X` → Error
/// - `X.Y` → Error
/// - `X.Y.Z` → `X.Y.Z` (pass-through, including existing build metadata)
pub fn normalize_version(version_str: &str, build: &Option<String>) -> Result<String> {
    let version = Version::parse(version_str).ok_or_else(|| anyhow::anyhow!("cannot parse version '{version_str}'"))?;

    if !version.has_patch() {
        bail!("version '{version_str}' needs full X.Y.Z format");
    }

    match build {
        Some(build) => {
            if version.has_build() {
                bail!("version '{version_str}' already has build metadata");
            }
            let with_build = if let Some(pre) = version.prerelease() {
                Version::new_prerelease_with_build(
                    version.major(),
                    version.minor().expect("has_patch guarantees minor"),
                    version.patch().expect("has_patch guarantees patch"),
                    pre,
                    build,
                )
            } else {
                Version::new_build(
                    version.major(),
                    version.minor().expect("has_patch guarantees minor"),
                    version.patch().expect("has_patch guarantees patch"),
                    build,
                )
            };
            Ok(with_build.to_string())
        }
        None => Ok(version.to_string()),
    }
}

/// The published tag for one env-source (`pylock`/`pypi`) version.
///
/// Same stamping rule as [`normalize_version`], with one difference: a version
/// that rule rejects keeps its bare form instead of being dropped. The archive
/// path can afford to skip an unnormalizable version (`plan.rs` filters it out
/// with `if let Ok(..)`) because a regex-resolved release tag is semver by
/// construction; an env source's version comes from PyPI, where a >3-component
/// release (`0.0.0.2`) or a `.dev0` suffix is ordinary and `ocx_lib::Version`
/// reads none of them — dropping those would stop the mirror publishing them at
/// all. Bare is also exactly what the rest of the env pipeline already does
/// with such a version: `push` gates `--cascade` on the same `Version::parse`,
/// so a version that cannot carry a stamp cannot carry rolling tags either.
pub fn env_version_tag(source_version: &str, build: &Option<String>) -> String {
    normalize_version(source_version, build).unwrap_or_else(|_| source_version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> Option<String> {
        Some("20260310142359".to_string())
    }

    #[test]
    fn normalize_patch() {
        assert_eq!(normalize_version("3.28.0", &ts()).unwrap(), "3.28.0_20260310142359");
    }

    #[test]
    fn reject_minor_only() {
        assert!(normalize_version("3.28", &ts()).is_err());
    }

    #[test]
    fn normalize_prerelease() {
        assert_eq!(
            normalize_version("3.28.0-rc1", &ts()).unwrap(),
            "3.28.0-rc1_20260310142359"
        );
    }

    #[test]
    fn reject_major_only() {
        assert!(normalize_version("3", &ts()).is_err());
    }

    #[test]
    fn reject_existing_build_with_timestamp() {
        assert!(normalize_version("3.28.0+existing", &ts()).is_err());
    }

    #[test]
    fn passthrough_existing_build_without_timestamp() {
        assert_eq!(normalize_version("25.0.2_10001", &None).unwrap(), "25.0.2_10001");
    }

    #[test]
    fn normalize_no_timestamp_patch() {
        assert_eq!(normalize_version("3.28.0", &None).unwrap(), "3.28.0");
    }

    #[test]
    fn reject_no_timestamp_minor() {
        assert!(normalize_version("3.28", &None).is_err());
    }

    #[test]
    fn date_format_timestamp() {
        let ts = build_timestamp(&BuildTimestampFormat::Date).unwrap();
        assert_eq!(ts.len(), 8); // YYYYMMDD
        assert!(ts.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn datetime_format_timestamp() {
        let ts = build_timestamp(&BuildTimestampFormat::Datetime).unwrap();
        assert_eq!(ts.len(), 14); // YYYYMMDDHHmmss
        assert!(ts.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn none_format_timestamp() {
        assert!(build_timestamp(&BuildTimestampFormat::None).is_none());
    }

    #[test]
    fn env_tag_stamps_what_it_can_and_keeps_the_rest_bare() {
        assert_eq!(env_version_tag("1.16.6", &ts()), "1.16.6_20260310142359");
        assert_eq!(env_version_tag("1.16.6", &None), "1.16.6");
        // PEP 440 releases `ocx_lib::Version` cannot read stay bare rather
        // than being dropped — they are ordinary on PyPI.
        assert_eq!(env_version_tag("0.0.0.2", &ts()), "0.0.0.2");
        assert_eq!(env_version_tag("2.0.0.dev0", &ts()), "2.0.0.dev0");
    }
}
