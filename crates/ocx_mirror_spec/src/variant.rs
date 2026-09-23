// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use serde::Deserialize;

use super::asset_type::AssetTypeConfig;
use super::assets::AssetPatterns;
use super::bin_scan::BinScanMode;
use super::metadata_config::MetadataConfig;

/// A variant declaration in a mirror spec.
///
/// Each variant has its own asset patterns (the primary differentiator) and
/// can optionally override metadata and asset_type from the top-level spec.
///
/// The `name` field is optional: the default variant may omit it to produce
/// bare (unprefixed) version tags. Non-default variants must have a name.
///
/// Variants are an archive-source surface (`github_release`/`url_index`).
/// Env-package sources (`pylock`/`pypi`) reject `variants:` entirely — their
/// per-platform wheel selection lives in the top-level `wheels:` map, and libc
/// is a platform `os.features` axis there, not a variant axis. Enforced
/// source-aware in `MirrorSpec::validate`, not by the type alone.
///
/// `deny_unknown_fields` matches [`MirrorSpec`](super::MirrorSpec). Without it
/// a misspelled key here is silently dropped while the same misspelling one
/// level up is a hard error — and `libc-lint: false` (the hyphenated spelling
/// `ocx`'s `--no-libc-lint` flag puts in front of operators) would then leave
/// the check on, the build still refusing, and the escape hatch looking broken
/// with no diagnostic.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default: bool,
    pub assets: AssetPatterns,
    #[serde(default)]
    pub metadata: Option<MetadataConfig>,
    #[serde(default)]
    pub asset_type: Option<AssetTypeConfig>,
    #[serde(default)]
    pub bin_scan: Option<BinScanMode>,
    #[serde(default)]
    pub libc_lint: Option<bool>,
}

/// A resolved variant with all inherited fields materialized.
///
/// Produced by [`MirrorSpec::effective_variants()`](super::MirrorSpec::effective_variants).
/// For legacy specs without a `variants` key, a single `EffectiveVariant` with
/// `name: None` is produced from the top-level fields.
#[derive(Debug, Clone)]
pub struct EffectiveVariant {
    /// Variant name, or `None` for legacy no-variant specs.
    pub name: Option<String>,
    /// Whether this is the default variant (always true for legacy specs).
    pub is_default: bool,
    /// Asset patterns for this variant.
    pub assets: AssetPatterns,
    /// Metadata config (variant override or inherited from top-level).
    pub metadata: Option<MetadataConfig>,
    /// Asset type config (variant override or inherited from top-level).
    pub asset_type: Option<AssetTypeConfig>,
    /// Binary auto-detection mode (variant override or inherited from
    /// top-level). A slim variant ships a different binary set than the full
    /// one, so it may want a different mode than the spec's.
    pub bin_scan: BinScanMode,
    /// Whether the libc check runs for this variant (variant override or
    /// inherited from top-level). One variant's upstream build can be the only
    /// one the check misreads, and bypassing the whole spec to get it through
    /// would silently stop checking the others.
    pub libc_lint: bool,
}
