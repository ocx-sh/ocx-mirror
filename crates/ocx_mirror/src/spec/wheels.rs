// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;

use ocx_lib::oci::{OperatingSystem, Platform};
use serde::Deserialize;
use serde::de::{self, Deserializer};

/// Per-platform wheel selection filters for env-package sources
/// (`source.type: pylock`/`pypi`) — the env analogue of
/// [`AssetPatterns`](super::AssetPatterns).
///
/// Keys are OCI platform strings, optionally carrying one
/// `+libc.glibc`/`+libc.musl` suffix (parsed via `Platform::from_str` into
/// `os_features`). The key is published **verbatim** as the image-index
/// platform entry — a declaration of the maintainer's support envelope, never
/// computed from wheel contents (a plain `linux/amd64` key with an explicit
/// `["manylinux", "any"]` filter is a legitimate glibc-only-but-unstamped
/// package). Values are ordered wheel platform-tag prefix lists acting as
/// admissibility filter + ranking; `~`/null selects the key-derived default
/// (see [`effective_filter`](Self::effective_filter)).
#[derive(Debug, Clone)]
pub struct WheelPatterns {
    pub filters: HashMap<Platform, Option<Vec<String>>>,
}

impl<'de> Deserialize<'de> for WheelPatterns {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: HashMap<String, Option<Vec<String>>> = HashMap::deserialize(deserializer)?;
        let mut filters = HashMap::with_capacity(raw.len());
        for (key, value) in raw {
            let platform: Platform = key
                .parse()
                .map_err(|_| de::Error::custom(format!("invalid platform '{key}'")))?;
            filters.insert(platform, value);
        }
        Ok(Self { filters })
    }
}

/// Wheel platform-tag prefix entries: lowercase letter, then lowercase
/// letters, digits, underscores, or dots (`any`, `manylinux_2_28`).
static FILTER_ENTRY_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z][a-z0-9_.]*$").unwrap());

/// The `libc.*` feature of a wheels key, when it declares one.
pub fn libc_feature(platform: &Platform) -> Option<&str> {
    match platform {
        Platform::Specific { os_features, .. } => os_features.iter().map(String::as_str).next(),
        Platform::Any => None,
    }
}

/// The base `os/arch` key of a wheels platform key — what `platforms:` (the CI
/// matrix), applicability windows, and the JUnit slug are keyed by. Wheels keys
/// carry no `variant`/`os_version` segments (validated), so this is the
/// canonical string with any `+libc.*` suffix stripped.
pub fn base_platform_key(platform: &Platform) -> String {
    match platform {
        Platform::Specific { os, arch, .. } => format!("{os}/{arch}"),
        Platform::Any => "any".to_string(),
    }
}

impl WheelPatterns {
    pub fn validate(&self, errors: &mut Vec<String>) {
        for (platform, filter) in &self.filters {
            let Platform::Specific {
                os,
                variant,
                os_version,
                os_features,
                ..
            } = platform
            else {
                errors.push("wheels: platform key 'any' is not supported (declare concrete os/arch keys)".to_string());
                continue;
            };
            if variant.is_some() || os_version.is_some() {
                errors.push(format!(
                    "wheels.{platform}: OCI variant/os_version segments are not supported in wheels keys"
                ));
            }
            if os_features.len() > 1 {
                errors.push(format!(
                    "wheels.{platform}: at most one libc feature per key (declare two keys instead)"
                ));
            }
            for feature in os_features {
                if !matches!(feature.as_str(), "libc.glibc" | "libc.musl") {
                    errors.push(format!(
                        "wheels.{platform}: unsupported platform feature '{feature}' (only libc.glibc/libc.musl)"
                    ));
                }
                if *os != OperatingSystem::Linux {
                    errors.push(format!("wheels.{platform}: libc features are only valid on linux keys"));
                }
            }

            let Some(filter) = filter else { continue };
            if filter.is_empty() {
                errors.push(format!(
                    "wheels.{platform}: filter must not be empty (omit the value for the key-derived default)"
                ));
            }
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for entry in filter {
                if !FILTER_ENTRY_RE.is_match(entry) {
                    errors.push(format!(
                        "wheels.{platform}: filter entry '{entry}' must match '^[a-z][a-z0-9_.]*$'"
                    ));
                }
                if !seen.insert(entry.as_str()) {
                    errors.push(format!("wheels.{platform}: duplicate filter entry '{entry}'"));
                }
            }
            // One uv tag target per entry: a single env cannot admit both
            // glibc and musl compiled wheels (it would require BOTH libcs at
            // runtime), and the key's declared libc must not contradict the
            // filter's wheel class.
            let has_manylinux = filter.iter().any(|entry| entry.starts_with("manylinux"));
            let has_musllinux = filter.iter().any(|entry| entry.starts_with("musllinux"));
            if has_manylinux && has_musllinux {
                errors.push(format!(
                    "wheels.{platform}: filter must not mix manylinux and musllinux prefixes"
                ));
            }
            match libc_feature(platform) {
                Some("libc.glibc") if has_musllinux => {
                    errors.push(format!(
                        "wheels.{platform}: musllinux entries contradict the key's libc.glibc feature"
                    ));
                }
                Some("libc.musl") if has_manylinux => {
                    errors.push(format!(
                        "wheels.{platform}: manylinux entries contradict the key's libc.musl feature"
                    ));
                }
                _ => {}
            }
        }
    }

    /// The effective filter for `platform`: the explicit list when declared,
    /// else the key-derived default. Plain linux keys default to `["any"]` —
    /// fail closed: a lock demanding a compiled wheel errors unless the
    /// maintainer overrides the filter (or declares a `+libc.*` key).
    pub fn effective_filter(&self, platform: &Platform) -> Vec<String> {
        if let Some(Some(filter)) = self.filters.get(platform) {
            return filter.clone();
        }
        let default: &[&str] = match platform {
            Platform::Specific { os, .. } => match (os, libc_feature(platform)) {
                (OperatingSystem::Linux, Some("libc.musl")) => &["musllinux", "any"],
                (OperatingSystem::Linux, Some("libc.glibc")) => &["manylinux", "any"],
                (OperatingSystem::Linux, _) => &["any"],
                (OperatingSystem::Darwin, _) => &["macosx", "any"],
                (OperatingSystem::Windows, _) => &["win", "any"],
            },
            Platform::Any => &["any"],
        };
        default.iter().map(ToString::to_string).collect()
    }

    /// Keys sorted by canonical platform string — the deterministic iteration
    /// order for plan/prepare legs.
    pub fn sorted_platforms(&self) -> Vec<&Platform> {
        let mut platforms: Vec<&Platform> = self.filters.keys().collect();
        platforms.sort_by_key(|platform| platform.to_string());
        platforms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> WheelPatterns {
        serde_yaml_ng::from_str(yaml).expect("valid wheel patterns")
    }

    fn validate(yaml: &str) -> Vec<String> {
        let mut errors = Vec::new();
        parse(yaml).validate(&mut errors);
        errors
    }

    fn key(patterns: &WheelPatterns) -> &Platform {
        patterns.filters.keys().next().expect("one entry")
    }

    #[test]
    fn libc_key_parses_into_os_features() {
        let patterns = parse("\"linux/amd64+libc.musl\": ~\n");
        match key(&patterns) {
            Platform::Specific { os_features, .. } => {
                assert_eq!(os_features, &vec!["libc.musl".to_string()]);
            }
            other => panic!("expected specific platform, got {other:?}"),
        }
    }

    #[test]
    fn glibc_and_musl_keys_coexist_as_distinct_entries() {
        let patterns = parse("\"linux/amd64+libc.glibc\": ~\n\"linux/amd64+libc.musl\": ~\n");
        assert_eq!(patterns.filters.len(), 2);
        assert!(validate("\"linux/amd64+libc.glibc\": ~\n\"linux/amd64+libc.musl\": ~\n").is_empty());
    }

    #[test]
    fn effective_filter_defaults_by_key_class() {
        let patterns = parse(concat!(
            "linux/amd64: ~\n",
            "\"linux/arm64+libc.glibc\": ~\n",
            "\"linux/arm64+libc.musl\": ~\n",
            "darwin/arm64: ~\n",
            "windows/amd64: ~\n",
        ));
        let by_string = |wanted: &str| {
            patterns
                .filters
                .keys()
                .find(|platform| platform.to_string() == wanted)
                .expect("key present")
        };
        assert_eq!(patterns.effective_filter(by_string("linux/amd64")), vec!["any"]);
        assert_eq!(
            patterns.effective_filter(by_string("linux/arm64+libc.glibc")),
            vec!["manylinux", "any"]
        );
        assert_eq!(
            patterns.effective_filter(by_string("linux/arm64+libc.musl")),
            vec!["musllinux", "any"]
        );
        assert_eq!(
            patterns.effective_filter(by_string("darwin/arm64")),
            vec!["macosx", "any"]
        );
        assert_eq!(
            patterns.effective_filter(by_string("windows/amd64")),
            vec!["win", "any"]
        );
    }

    #[test]
    fn explicit_filter_wins_over_default() {
        // The gnu-under-plain-key case: maintainer's support envelope, no
        // stamping, no warning.
        let patterns = parse("linux/amd64: [manylinux, any]\n");
        assert_eq!(patterns.effective_filter(key(&patterns)), vec!["manylinux", "any"]);
        assert!(validate("linux/amd64: [manylinux, any]\n").is_empty());
    }

    #[test]
    fn rejects_any_key_and_variant_segments() {
        assert!(
            validate("any: ~\n")
                .iter()
                .any(|e| e.contains("'any' is not supported"))
        );
        assert!(
            validate("linux/arm64/v8: ~\n")
                .iter()
                .any(|e| e.contains("variant/os_version"))
        );
    }

    #[test]
    fn rejects_dual_libc_and_unknown_features() {
        assert!(
            validate("\"linux/amd64+libc.glibc+libc.musl\": ~\n")
                .iter()
                .any(|e| e.contains("at most one libc feature"))
        );
        assert!(
            validate("\"linux/amd64+win32k\": ~\n")
                .iter()
                .any(|e| e.contains("unsupported platform feature 'win32k'"))
        );
        assert!(
            validate("\"darwin/arm64+libc.glibc\": ~\n")
                .iter()
                .any(|e| e.contains("only valid on linux"))
        );
    }

    #[test]
    fn rejects_empty_filter_bad_entries_and_duplicates() {
        assert!(
            validate("linux/amd64: []\n")
                .iter()
                .any(|e| e.contains("must not be empty"))
        );
        assert!(
            validate("linux/amd64: [\"Any\"]\n")
                .iter()
                .any(|e| e.contains("must match"))
        );
        assert!(
            validate("linux/amd64: [any, any]\n")
                .iter()
                .any(|e| e.contains("duplicate filter entry"))
        );
    }

    #[test]
    fn rejects_mixed_and_contradicting_wheel_classes() {
        assert!(
            validate("linux/amd64: [manylinux, musllinux, any]\n")
                .iter()
                .any(|e| e.contains("must not mix"))
        );
        assert!(
            validate("\"linux/amd64+libc.glibc\": [musllinux, any]\n")
                .iter()
                .any(|e| e.contains("contradict the key's libc.glibc"))
        );
        assert!(
            validate("\"linux/amd64+libc.musl\": [manylinux_2_28, any]\n")
                .iter()
                .any(|e| e.contains("contradict the key's libc.musl"))
        );
    }

    #[test]
    fn sorted_platforms_orders_by_canonical_string() {
        let patterns = parse(concat!(
            "\"linux/amd64+libc.musl\": ~\n",
            "linux/amd64: ~\n",
            "\"linux/amd64+libc.glibc\": ~\n",
        ));
        let order: Vec<String> = patterns.sorted_platforms().iter().map(|p| p.to_string()).collect();
        assert_eq!(
            order,
            vec!["linux/amd64", "linux/amd64+libc.glibc", "linux/amd64+libc.musl"]
        );
    }
}
