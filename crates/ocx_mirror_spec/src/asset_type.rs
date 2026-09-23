// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;

use serde::Deserialize;

/// Describes how downloaded assets should be processed before bundling.
///
/// Archives are extracted (with optional component stripping). Raw binaries are
/// placed directly into the content directory under the configured name.
///
/// Supports three YAML forms:
///
/// Uniform archive (all platforms):
/// ```yaml
/// asset_type:
///   type: archive
///   strip_components: 1
/// ```
///
/// Uniform archive with per-platform strip:
/// ```yaml
/// asset_type:
///   type: archive
///   strip_components:
///     default: 1
///     platforms:
///       windows/amd64: 0
/// ```
///
/// Uniform binary (all platforms):
/// ```yaml
/// asset_type:
///   type: binary
///   name: shfmt
/// ```
///
/// Per-platform mix — e.g. archive on Linux/macOS, raw binary on Windows:
/// ```yaml
/// asset_type:
///   default:
///     type: archive
///     strip_components: 0
///   platforms:
///     windows/amd64:
///       type: binary
///       name: lychee
/// ```
#[derive(Debug, Clone)]
pub enum AssetTypeConfig {
    /// One asset type for all platforms.
    Uniform(UniformAssetType),
    /// Per-platform override map with a default fallback.
    PerPlatform(PerPlatformAssetType),
}

/// The `{default, platforms}` form of [`AssetTypeConfig`].
///
/// A named struct rather than an enum variant because `deny_unknown_fields` is
/// a container attribute and serde has no variant-level spelling of it. That
/// matters here more than anywhere: without it, the flat form — the per-platform
/// map written one level too high —
///
/// ```yaml
/// asset_type:
///   default: { type: binary, name: claude }
///   windows/amd64: { type: binary, name: claude.exe }   # no `platforms:`
/// ```
///
/// parsed as `platforms: {}` and every override was dropped on the floor. The
/// spec validated, the pipeline ran, and the Windows bundle shipped a binary
/// named `claude`. Nothing said a word until someone opened the tarball.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerPlatformAssetType {
    pub default: UniformAssetType,
    #[serde(default)]
    pub platforms: HashMap<String, UniformAssetType>,
}

/// Hand-rolled rather than `#[serde(untagged)]`, for the same reason
/// [`super::platforms_config`]'s `runner` is: an untagged enum renders every
/// failure as "data did not match any variant", identically for a stray key,
/// a bogus inner `type:` and a scalar where a mapping belongs. The key that
/// picks the form is unambiguous — `type:` or `default:` — so dispatching on
/// it lets the real error through, which is the whole point of refusing the
/// input at all.
impl<'de> serde::Deserialize<'de> for AssetTypeConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        let Some(mapping) = value.as_mapping() else {
            return Err(D::Error::custom(
                "asset_type must be a mapping: `type:` for one type across every platform, \
                 or `default:` with an optional `platforms:` map",
            ));
        };

        if mapping.contains_key("type") {
            // `UniformAssetType` is `#[serde(tag = "type")]`, and a tagged enum
            // has no `deny_unknown_fields` to give it — so a `default:` or
            // `platforms:` sitting beside `type:` was dropped in silence, the
            // same defect as the flat form below and just as invisible. The
            // two forms are mutually exclusive by construction; say so.
            if let Some(stray) = ["default", "platforms"]
                .into_iter()
                .find(|key| mapping.contains_key(*key))
            {
                return Err(D::Error::custom(format!(
                    "asset_type: `{stray}:` cannot sit beside `type:` — `type:` is the one-type-for-every-platform \
                     form, so per-platform overrides go under `default:` with a `platforms:` map instead"
                )));
            }
            serde_yaml_ng::from_value(value)
                .map(Self::Uniform)
                .map_err(D::Error::custom)
        } else if mapping.contains_key("default") {
            serde_yaml_ng::from_value(value)
                .map(Self::PerPlatform)
                .map_err(|error| {
                    // Labelled, because the inner error names only the stray key.
                    // `asset_type:`, `metadata:` and a nested `strip_components:`
                    // all reject the same misplaced platform key, so an unlabelled
                    // "unknown field `windows/amd64`" left the reader to find which
                    // block of the spec it came from. Label only: serde's own
                    // "expected `default` or `platforms`" is already the advice,
                    // and a nested `strip_components:` failure arrives here
                    // self-labelled, so a second sentence printed it twice.
                    D::Error::custom(format!("asset_type: {error}"))
                })
        } else {
            Err(D::Error::custom(
                "asset_type needs either `type:` (one type for every platform) or `default:` \
                 (a per-platform map, whose overrides go under `platforms:`)",
            ))
        }
    }
}

/// A single asset type definition, used either on its own or as the default /
/// per-platform value inside [`AssetTypeConfig::PerPlatform`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UniformAssetType {
    /// The asset is an archive (tar, zip, etc.) to be extracted.
    Archive {
        /// Leading path components to strip from the archive before rebundling.
        #[serde(default)]
        strip_components: Option<super::StripComponentsConfig>,
    },
    /// The asset is a standalone executable binary.
    Binary {
        /// The filename for the binary in the package (e.g. `shfmt`).
        /// On Windows, `.exe` is appended automatically if the downloaded asset has it.
        name: String,
    },
}

impl UniformAssetType {
    fn resolve(&self, platform: &str) -> AssetType {
        match self {
            Self::Archive { strip_components } => {
                let strip = strip_components.as_ref().and_then(|sc| sc.resolve(platform));
                AssetType::Archive {
                    strip_components: strip,
                }
            }
            Self::Binary { name } => AssetType::Binary { name: name.clone() },
        }
    }
}

impl AssetTypeConfig {
    /// The keys of the `platforms:` override map, empty for the uniform form.
    ///
    /// Exists so `MirrorSpec::validate` can check them against the platform
    /// grammar without matching on the variant.
    pub fn platform_keys(&self) -> impl Iterator<Item = &String> {
        match self {
            Self::Uniform(_) => None,
            Self::PerPlatform(per_platform) => Some(per_platform.platforms.keys()),
        }
        .into_iter()
        .flatten()
    }

    /// Every key of every nested `strip_components.platforms:` map.
    ///
    /// The second per-platform map an `asset_type:` block can carry, one level
    /// further down — `archive` entries take their own `strip_components:` in
    /// either the scalar or the `{default, platforms}` form. Its keys are
    /// looked up by the same exact-string equality and were unchecked while the
    /// outer map's were not, so `windows/amd65:` here silently fell through to
    /// `default` exactly as issue #86 describes.
    pub fn strip_components_platform_keys(&self) -> Vec<&String> {
        fn keys_of(uniform: &UniformAssetType) -> Vec<&String> {
            match uniform {
                UniformAssetType::Archive {
                    strip_components: Some(super::StripComponentsConfig::PerPlatform(per_platform)),
                } => per_platform.platforms.keys().collect(),
                _ => Vec::new(),
            }
        }

        match self {
            Self::Uniform(uniform) => keys_of(uniform),
            Self::PerPlatform(per_platform) => keys_of(&per_platform.default)
                .into_iter()
                .chain(per_platform.platforms.values().flat_map(keys_of))
                .collect(),
        }
    }

    /// Resolve to a concrete [`AssetType`] for a specific platform.
    pub fn resolve(&self, platform: &str) -> AssetType {
        match self {
            Self::Uniform(u) => u.resolve(platform),
            Self::PerPlatform(per_platform) => per_platform
                .platforms
                .get(platform)
                .unwrap_or(&per_platform.default)
                .resolve(platform),
        }
    }
}

/// Resolved asset type for a specific platform, ready for the pipeline.
#[derive(Debug, Clone)]
pub enum AssetType {
    Archive { strip_components: Option<u8> },
    Binary { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── issue #86: the flat form is a typo, not a spelling ──────────────

    #[test]
    fn a_flat_per_platform_map_is_refused() {
        // The override written one level too high. This validated, published
        // `claude` on every platform including Windows, and said nothing —
        // the stray key landed in no declared field and serde dropped it.
        let yaml = r#"
default:
  type: binary
  name: claude
windows/amd64:
  type: binary
  name: claude.exe
"#;
        let error = serde_yaml_ng::from_str::<AssetTypeConfig>(yaml)
            .expect_err("the flat form must be refused, not silently ignored");
        let message = error.to_string();
        assert!(
            message.contains("windows/amd64"),
            "the message must name the key that has nowhere to go: {message}"
        );
    }

    #[test]
    fn a_uniform_type_carrying_per_platform_keys_is_refused() {
        // The flat form's mirror image, and the shape the first fix missed:
        // `type:` wins the dispatch, `UniformAssetType` is `#[serde(tag =
        // "type")]` with no `deny_unknown_fields` to give it, and the sibling
        // `platforms:` map was swallowed exactly like the flat form's stray
        // key — same silent drop, same wrong bundle, one key different.
        for yaml in [
            r#"
type: archive
platforms:
  windows/amd64:
    type: binary
    name: claude.exe
"#,
            r#"
type: archive
default:
  type: binary
  name: claude
"#,
        ] {
            let error = serde_yaml_ng::from_str::<AssetTypeConfig>(yaml)
                .expect_err("a uniform type cannot also carry per-platform overrides");
            let message = error.to_string();
            assert!(
                message.contains("asset_type") && message.contains("platforms:"),
                "the message must say where the overrides belong: {message}"
            );
        }
    }

    #[test]
    fn a_bogus_inner_type_inside_the_platforms_map_is_refused() {
        // Never reached before: the whole map was discarded, so its values
        // were never parsed and `type: bogus` cost nothing.
        let yaml = r#"
default:
  type: binary
  name: claude
platforms:
  windows/amd64:
    type: bogus
"#;
        let error = serde_yaml_ng::from_str::<AssetTypeConfig>(yaml).expect_err("`bogus` is not an asset type");
        assert!(
            error.to_string().contains("bogus"),
            "the message must name the bad variant: {error}"
        );
    }

    #[test]
    fn a_block_with_neither_type_nor_default_says_which_it_needs() {
        let error =
            serde_yaml_ng::from_str::<AssetTypeConfig>("platforms:\n  linux/amd64:\n    type: binary\n    name: x\n")
                .expect_err("`platforms:` alone is neither form");
        let message = error.to_string();
        assert!(
            message.contains("`type:`") && message.contains("`default:`"),
            "unhelpful message: {message}"
        );
    }

    #[test]
    fn deserialize_archive_uniform() {
        let yaml = r#"
type: archive
strip_components: 1
"#;
        let config: AssetTypeConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.resolve("linux/amd64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(1)),
            _ => panic!("expected Archive"),
        }
    }

    #[test]
    fn deserialize_archive_per_platform_strip() {
        let yaml = r#"
type: archive
strip_components:
  default: 1
  platforms:
    windows/amd64: 0
"#;
        let config: AssetTypeConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.resolve("linux/amd64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(1)),
            _ => panic!("expected Archive"),
        }
        match config.resolve("windows/amd64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(0)),
            _ => panic!("expected Archive"),
        }
    }

    #[test]
    fn deserialize_archive_no_strip() {
        let yaml = r#"
type: archive
"#;
        let config: AssetTypeConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.resolve("linux/amd64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, None),
            _ => panic!("expected Archive"),
        }
    }

    #[test]
    fn deserialize_binary() {
        let yaml = r#"
type: binary
name: shfmt
"#;
        let config: AssetTypeConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.resolve("linux/amd64") {
            AssetType::Binary { name } => assert_eq!(name, "shfmt"),
            _ => panic!("expected Binary"),
        }
    }

    #[test]
    fn deserialize_per_platform_mix_archive_and_binary() {
        let yaml = r#"
default:
  type: archive
  strip_components: 0
platforms:
  windows/amd64:
    type: binary
    name: lychee
"#;
        let config: AssetTypeConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.resolve("linux/amd64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(0)),
            _ => panic!("expected Archive for linux/amd64"),
        }
        match config.resolve("darwin/arm64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(0)),
            _ => panic!("expected Archive for darwin/arm64"),
        }
        match config.resolve("windows/amd64") {
            AssetType::Binary { name } => assert_eq!(name, "lychee"),
            _ => panic!("expected Binary for windows/amd64"),
        }
    }

    #[test]
    fn per_platform_falls_back_to_default_for_unmatched_platform() {
        let yaml = r#"
default:
  type: binary
  name: tool
platforms:
  linux/amd64:
    type: archive
    strip_components: 1
"#;
        let config: AssetTypeConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.resolve("linux/amd64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(1)),
            _ => panic!("expected Archive for linux/amd64"),
        }
        match config.resolve("darwin/arm64") {
            AssetType::Binary { name } => assert_eq!(name, "tool"),
            _ => panic!("expected Binary fallback for darwin/arm64"),
        }
    }

    #[test]
    fn per_platform_without_explicit_platforms_key_uses_default() {
        let yaml = r#"
default:
  type: archive
  strip_components: 2
"#;
        let config: AssetTypeConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.resolve("linux/amd64") {
            AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(2)),
            _ => panic!("expected Archive"),
        }
    }
}
