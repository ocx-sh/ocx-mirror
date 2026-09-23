// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::fmt;

use serde::Deserialize;
use serde::de;

/// What a publisher-declared asset digest obliges the download to do.
///
/// Spelled either as a bool — the pre-#76 form, which every spec in the fleet
/// still carries — or as one of the three names:
///
/// ```yaml
/// verify:
///   github_asset_digest: require   # or `true` (= if_present) / `false` (= off)
///   url_index_digest: if_present
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DigestPolicy {
    /// Ignore any declared digest.
    Off,
    /// Verify when the source declared one; a source that declares none is
    /// fine. The default on both axes.
    #[default]
    IfPresent,
    /// Verify, and fail the asset when the source declared none — the guard
    /// against a `sha265:` typo in a generator quietly degrading the mirror to
    /// "no digest".
    Require,
}

impl DigestPolicy {
    /// The digest to hand the download, under this policy: `Off` drops it, so
    /// `pipeline::verify::verify` needs no policy of its own.
    pub fn apply(self, declared: Option<&String>) -> Option<String> {
        match self {
            Self::Off => None,
            Self::IfPresent | Self::Require => declared.cloned(),
        }
    }

    /// Whether a *missing* declared digest fails the asset.
    pub fn requires_digest(self) -> bool {
        self == Self::Require
    }
}

/// Deserialized through a visitor rather than an `#[serde(untagged)]` enum for
/// the reason `cascade_config.rs` states: untagged reports every failure as
/// "data did not match any variant", swallowing the diagnostic that names what
/// the operator actually mistyped.
impl<'de> Deserialize<'de> for DigestPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(DigestPolicyVisitor)
    }
}

struct DigestPolicyVisitor;

const POLICY_NAMES: &[&str] = &["off", "if_present", "require"];

impl de::Visitor<'_> for DigestPolicyVisitor {
    type Value = DigestPolicy;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(r#"a boolean, or one of "off", "if_present", "require""#)
    }

    /// The pre-#76 spelling, byte-identical in behaviour: `true` verified a
    /// declared digest and tolerated its absence, `false` verified nothing.
    fn visit_bool<E: de::Error>(self, enabled: bool) -> Result<Self::Value, E> {
        Ok(match enabled {
            true => DigestPolicy::IfPresent,
            false => DigestPolicy::Off,
        })
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        match value {
            "off" => Ok(DigestPolicy::Off),
            "if_present" => Ok(DigestPolicy::IfPresent),
            "require" => Ok(DigestPolicy::Require),
            other => Err(de::Error::unknown_variant(other, POLICY_NAMES)),
        }
    }
}

/// Integrity checks run against every downloaded asset.
///
/// `deny_unknown_fields` because this is a three-key hand-written block: a
/// `sha265_file:` would otherwise sit in a committed spec verifying nothing,
/// which is the failure mode the block exists to prevent.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyConfig {
    /// Digests the GitHub Releases API declares per asset (`Asset.digest`).
    /// Absent on releases published before GitHub added the field.
    #[serde(default)]
    pub github_asset_digest: DigestPolicy,
    /// Digests a url_index document declares per asset, in the object asset
    /// form (`{url, sha256}`).
    #[serde(default)]
    pub url_index_digest: DigestPolicy,
    /// URL of a `sha256sum`-format sidecar listing the release's assets.
    pub checksums_file: Option<String>,
}
