// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use serde::Deserialize;

use super::asset_type::AssetTypeConfig;
use super::assets::AssetPatterns;
use super::metadata_config::MetadataConfig;

/// A variant declaration in a mirror spec.
///
/// Each variant has its own asset patterns (the primary differentiator) and
/// can optionally override metadata and asset_type from the top-level spec.
///
/// The `name` field is optional: the default variant may omit it to produce
/// bare (unprefixed) version tags. Non-default variants must have a name.
///
/// `assets` is required for `github_release`/`url_index` sources and
/// forbidden for `pylock`, which instead selects wheels via the constraint
/// fields below (`libc`, `min_manylinux`, `min_musllinux`, `abi` — the L1-fact
/// axis vocabulary from `design_spec_ocx_python.md`). Both rules are enforced
/// source-aware in `MirrorSpec::validate`, not by the type alone.
#[derive(Debug, Clone, Deserialize)]
pub struct VariantSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub assets: Option<AssetPatterns>,
    #[serde(default)]
    pub metadata: Option<MetadataConfig>,
    #[serde(default)]
    pub asset_type: Option<AssetTypeConfig>,

    /// Required libc family for `pylock` variant selection: `"gnu"` or `"musl"`.
    #[serde(default)]
    pub libc: Option<String>,
    /// Minimum manylinux floor (PEP 600), e.g. `"2_28"`.
    #[serde(default)]
    pub min_manylinux: Option<String>,
    /// Minimum musllinux floor (PEP 656), e.g. `"1_2"`.
    #[serde(default)]
    pub min_musllinux: Option<String>,
    /// Required CPython ABI tag, e.g. `"cp313t"` for the free-threaded build.
    #[serde(default)]
    pub abi: Option<String>,

    /// Per-variant OCX interpreter package override for `pylock` (e.g. a
    /// musl-libc CPython build for a `libc: musl` variant). Falls back to
    /// `python.interpreter_package` when unset.
    #[serde(default)]
    pub interpreter_package: Option<String>,
}

static MANYLINUX_FLOOR_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^\d+_\d+$").unwrap());
static ABI_TAG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^cp\d+t?$").unwrap());

impl VariantSpec {
    /// Whether this variant declares any `pylock` constraint field.
    pub fn has_python_constraints(&self) -> bool {
        self.libc.is_some() || self.min_manylinux.is_some() || self.min_musllinux.is_some() || self.abi.is_some()
    }

    /// Validate the `pylock` constraint fields' format. Cross-field
    /// consistency (e.g. resolving `libc` against the matching floor field
    /// during wheel selection) is the lock→VersionInfo adapter's concern.
    pub fn validate_python_constraints(&self, errors: &mut Vec<String>) {
        if let Some(libc) = &self.libc
            && !matches!(libc.as_str(), "gnu" | "musl")
        {
            errors.push(format!("variants: libc '{libc}' must be 'gnu' or 'musl'"));
        }
        if let Some(floor) = &self.min_manylinux
            && !MANYLINUX_FLOOR_RE.is_match(floor)
        {
            errors.push(format!(
                "variants: min_manylinux '{floor}' must match '<major>_<minor>' (e.g. '2_28')"
            ));
        }
        if let Some(floor) = &self.min_musllinux
            && !MANYLINUX_FLOOR_RE.is_match(floor)
        {
            errors.push(format!(
                "variants: min_musllinux '{floor}' must match '<major>_<minor>' (e.g. '1_2')"
            ));
        }
        if let Some(abi) = &self.abi
            && !ABI_TAG_RE.is_match(abi)
        {
            errors.push(format!(
                "variants: abi '{abi}' is not a valid CPython ABI tag (expected e.g. 'cp313' or 'cp313t')"
            ));
        }
    }
}

/// A resolved variant with all inherited fields materialized.
///
/// Produced by [`MirrorSpec::effective_variants()`](super::MirrorSpec::effective_variants).
/// For legacy specs without a `variants` key, a single `EffectiveVariant` with
/// `name: None` is produced from the top-level fields.
///
/// Asset-pattern resolution only: `effective_variants()` skips `pylock`
/// variants (constraint fields, no `assets`) rather than producing one with
/// empty patterns. The pylock adapter resolves its own variant→target mapping
/// directly from `VariantSpec`'s constraint fields instead.
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
}
