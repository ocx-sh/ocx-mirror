// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;

use serde::Deserialize;

/// Configuration for stripping leading path components from archives before rebundling.
///
/// Supports two YAML forms:
///
/// Simple (all platforms share the same value):
/// ```yaml
/// strip_components: 1
/// ```
///
/// Per-platform (with optional default):
/// ```yaml
/// strip_components:
///   default: 1
///   platforms:
///     windows/amd64: 0
/// ```
#[derive(Debug, Clone)]
pub enum StripComponentsConfig {
    Uniform(u8),
    PerPlatform(PerPlatformStripComponents),
}

/// The `{default, platforms}` form of [`StripComponentsConfig`].
///
/// A named struct so `deny_unknown_fields` can apply — it is a container
/// attribute, with no variant-level spelling. Both fields default, so the flat
/// form (`strip_components: { windows/amd64: 0 }`, the overrides written
/// without their `platforms:` wrapper) used to parse with *nothing* set: no
/// default, no overrides, every platform silently falling through to `None`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerPlatformStripComponents {
    #[serde(default)]
    pub default: Option<u8>,
    #[serde(default)]
    pub platforms: HashMap<String, u8>,
}

/// Hand-rolled rather than `#[serde(untagged)]`: an untagged enum answers a
/// stray key, a non-integer and a malformed map with one indistinguishable
/// "data did not match any variant". The two forms are a scalar and a mapping,
/// so telling them apart costs nothing and every error keeps its own message.
impl<'de> serde::Deserialize<'de> for StripComponentsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        if value.is_mapping() {
            serde_yaml_ng::from_value(value)
                .map(Self::PerPlatform)
                .map_err(|error| {
                    // Labelled for the same reason `asset_type`'s arm is: the inner
                    // error names the stray key and nothing else, and this block is
                    // nested inside an `asset_type:` entry, so it is the hardest of
                    // the three to locate from the message alone.
                    D::Error::custom(format!("strip_components: {error}"))
                })
        } else {
            serde_yaml_ng::from_value(value).map(Self::Uniform).map_err(|error| {
                D::Error::custom(format!(
                    "{error}; strip_components is a number, or a `default:`/`platforms:` map"
                ))
            })
        }
    }
}

impl StripComponentsConfig {
    /// Resolve the strip_components value for a specific platform.
    ///
    /// For `Uniform`, returns the same value for all platforms.
    /// For `PerPlatform`, checks the platform map first, then falls back to the default.
    pub fn resolve(&self, platform: &str) -> Option<u8> {
        match self {
            Self::Uniform(n) => Some(*n),
            Self::PerPlatform(per_platform) => per_platform.platforms.get(platform).copied().or(per_platform.default),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_resolves_same_for_all_platforms() {
        let config = StripComponentsConfig::Uniform(1);
        assert_eq!(config.resolve("linux/amd64"), Some(1));
        assert_eq!(config.resolve("windows/amd64"), Some(1));
    }

    #[test]
    fn per_platform_resolves_specific_platform() {
        let config = StripComponentsConfig::PerPlatform(PerPlatformStripComponents {
            default: Some(1),
            platforms: HashMap::from([("windows/amd64".to_string(), 0)]),
        });
        assert_eq!(config.resolve("linux/amd64"), Some(1));
        assert_eq!(config.resolve("windows/amd64"), Some(0));
    }

    #[test]
    fn per_platform_without_default_returns_none_for_unmatched() {
        let config = StripComponentsConfig::PerPlatform(PerPlatformStripComponents {
            default: None,
            platforms: HashMap::from([("windows/amd64".to_string(), 0)]),
        });
        assert_eq!(config.resolve("linux/amd64"), None);
        assert_eq!(config.resolve("windows/amd64"), Some(0));
    }

    #[test]
    fn a_flat_per_platform_map_is_refused() {
        // Worse than `asset_type`'s: both fields default, so the flat form
        // parsed with nothing set at all — no default, no overrides.
        let yaml = "windows/amd64: 0\n";
        let error = serde_yaml_ng::from_str::<StripComponentsConfig>(yaml)
            .expect_err("overrides written without `platforms:` must be refused");
        assert!(
            error.to_string().contains("windows/amd64"),
            "the message must name the stray key: {error}"
        );
    }

    #[test]
    fn deserialize_uniform() {
        let yaml = "1";
        let config: StripComponentsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.resolve("linux/amd64"), Some(1));
    }

    #[test]
    fn deserialize_per_platform() {
        let yaml = r#"
default: 1
platforms:
  windows/amd64: 0
"#;
        let config: StripComponentsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.resolve("linux/amd64"), Some(1));
        assert_eq!(config.resolve("windows/amd64"), Some(0));
    }
}
