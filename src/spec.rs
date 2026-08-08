// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

mod announce_config;
mod asset_type;
mod assets;
mod bin_scan;
mod cascade_config;
mod catalog_config;
mod concurrency_config;
mod load;
mod metadata_config;
mod notify_config;
mod ocx_mirror_config;
mod platform_keys;
mod platforms_config;
mod python_config;
mod source;
mod strip_components_config;
mod target;
mod tests_config;
mod validate;
mod variant;
mod verify_config;
mod versions_config;
mod wheels;

pub use announce_config::{AnnounceConfig, DEFAULT_INDEX_REPO};
pub use asset_type::{AssetType, AssetTypeConfig};
pub use assets::AssetPatterns;
pub use bin_scan::BinScanMode;
pub use cascade_config::CascadeConfig;
pub use catalog_config::CatalogConfig;
pub use concurrency_config::{ConcurrencyConfig, resolve_compression_threads};
#[allow(unused_imports)]
// Glob so `crate::spec::…` stays the one path callers use — the split is an
// internal file boundary, not a new namespace for them to learn — and so this
// module's own `impl MirrorSpec` keeps calling the validators unqualified.
// `pub(crate)`, not `pub`: the children's items are crate-internal, and a
// public glob would put every helper on the library's API surface.
pub(crate) use load::*;
pub use metadata_config::MetadataConfig;
#[allow(unused_imports)]
pub use notify_config::{DiscordConfig, NotifyConfig};
pub use ocx_mirror_config::OcxMirrorConfig;
pub(crate) use platform_keys::*;
#[allow(unused_imports)]
pub use platforms_config::{ContainerConfig, ExcludeEntry, PlatformConfig, Severity};
pub use python_config::{LockOptions, PythonConfig};
pub use source::{GeneratorConfig, Source, UrlIndexSource, UrlIndexVersion};
pub use strip_components_config::StripComponentsConfig;
pub use target::Target;
pub use tests_config::{TestEntry, TestKind};
pub(crate) use validate::*;
pub use variant::{EffectiveVariant, VariantSpec};
pub use verify_config::VerifyConfig;
pub(crate) use versions_config::BackfillOrder;
pub use versions_config::VersionsConfig;
pub use wheels::{WheelPatterns, base_platform_key, libc_feature};

use ocx_lib::package::version::Version;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorSpec {
    pub name: String,
    pub target: Target,
    pub source: Source,

    /// Interpreter configuration for env-package sources. Required when
    /// `source.type` is `pylock` or `pypi`; unused otherwise.
    #[serde(default)]
    pub python: Option<PythonConfig>,

    /// Wheel repo naming scope prefix for env-package sources — maps to
    /// `ocx_python::WheelScope`. Defaults to `"pip-packages"`.
    #[serde(default = "default_wheel_scope")]
    pub wheel_scope: String,

    /// Asset patterns for non-variant specs. Mutually exclusive with `variants`.
    /// Not supported for env-package sources (see [`MirrorSpec::validate`]).
    #[serde(default)]
    pub assets: Option<AssetPatterns>,

    /// Per-platform wheel selection filters — the env-source analogue of
    /// `assets`. Required for `source.type: pylock`/`pypi`; rejected
    /// otherwise. Keys (optionally `+libc.glibc`/`+libc.musl`-suffixed) are
    /// published verbatim as image-index platform entries.
    #[serde(default)]
    pub wheels: Option<WheelPatterns>,

    /// Variant declarations. Mutually exclusive with top-level `assets`.
    /// Each variant has its own asset patterns and can override `metadata`
    /// and `asset_type` from the top-level spec.
    #[serde(default)]
    pub variants: Option<Vec<VariantSpec>>,

    #[serde(default)]
    pub metadata: Option<MetadataConfig>,

    /// Whether the published `binaries` metadata claim is derived from the
    /// extracted content tree rather than hand-listed in the metadata file.
    ///
    /// `off` (the default) publishes exactly what the metadata file declares.
    /// `auto` fills an absent claim from the scan; `verify` also checks a
    /// declared one against the tree, so a hand-written list becomes a
    /// regression test against upstream rearranging its archive.
    ///
    /// Archive sources only: an env-package spec (`pylock`/`pypi`) declaring a
    /// scanning mode is rejected, because there is no extracted archive tree
    /// for the scan to walk (see [`MirrorSpec::validate`]).
    #[serde(default)]
    pub bin_scan: BinScanMode,

    /// Whether a Linux build's declared `os.features` are checked against the
    /// libc its packaged binaries actually link against.
    ///
    /// `true` (the default): a binary on the interface `PATH` needing a libc
    /// family the platform key does not declare fails that version's build.
    /// Under `os.features` subset matching an undeclared family is a positive
    /// claim of libc universality, so publishing one ships a tile that resolves
    /// onto hosts which cannot execute it.
    ///
    /// `false` bypasses the whole check, refusals and scan-scope failures
    /// alike — the same total bypass as `ocx package create --no-libc-lint`,
    /// and for the same reason: a false refusal would otherwise block every
    /// publish with no way through. A partial bypass would leave a bug in the
    /// un-bypassed half still able to stop a mirror.
    ///
    /// A boolean, not a [`BinScanMode`]-shaped enum: the check has two states
    /// and `ocx` spells it as one flag.
    ///
    /// On env-package sources this field is accepted but **inert**: the env
    /// prepare path deliberately does not run the lint — no extracted content
    /// tree exists, and the composed env's `PATH` vars are private-visibility,
    /// so the scan scope would be empty by construction (see the rationale
    /// block in `pipeline/python_prepare.rs`). Libc correctness for env
    /// packages is enforced on the input set instead: the `wheels:` key's
    /// `+libc.*` feature drives which manylinux/musllinux wheels are
    /// admissible.
    #[serde(default = "default_true")]
    pub libc_lint: bool,

    /// How to process downloaded assets before bundling.
    ///
    /// - `archive`: Extract the asset as a tar/zip archive, optionally stripping
    ///   leading path components (e.g. `strip_components: 1`).
    /// - `binary`: The asset is a standalone executable. Place it directly into
    ///   the content directory under the configured `name`.
    ///
    /// Defaults to `archive` with no stripping when omitted.
    #[serde(default)]
    pub asset_type: Option<AssetTypeConfig>,

    #[serde(default = "default_build_timestamp")]
    pub build_timestamp: BuildTimestampFormat,

    /// Rolling-tag cascade: `true`/`false`, or a map opting the generated
    /// repair workflow into a `schedule:` trigger (see [`CascadeConfig`]).
    #[serde(default)]
    pub cascade: CascadeConfig,

    #[serde(default)]
    pub versions: Option<VersionsConfig>,

    #[serde(default)]
    pub skip_prereleases: bool,

    #[serde(default)]
    pub verify: Option<VerifyConfig>,

    #[serde(default)]
    pub concurrency: ConcurrencyConfig,

    // ── Pipeline test configuration (added in test-pipeline phase) ──
    /// Test commands to run against each installed package before publishing.
    /// Required by `ocx-mirror push`; optional for backwards-compat parsing.
    #[serde(default)]
    pub tests: Option<Vec<TestEntry>>,

    /// Per-platform runner + container matrix for the generated GHA workflow.
    /// Keys use the canonical platform grammar `os/arch[/variant][+feature,…]`,
    /// so a libc claim (`linux/amd64+libc.musl`) is a first-class key here just
    /// as it is under `assets:`.
    #[serde(default)]
    pub platforms: Option<HashMap<String, PlatformConfig>>,

    /// Pins the `ocx-mirror` release tag (and optionally a git SHA) used
    /// when installing `ocx-mirror` and downloading the `ocx` binary inside
    /// the generated workflow.
    #[serde(default)]
    pub ocx_mirror: Option<OcxMirrorConfig>,

    /// Notification settings (currently only Discord webhooks).
    #[serde(default)]
    pub notify: Option<NotifyConfig>,

    /// Index announce settings. When present, a push run that published at
    /// least one version makes a single `ocx package announce` call carrying
    /// every cascade tag the run wrote. Absent → nothing is announced.
    #[serde(default)]
    pub announce: Option<AnnounceConfig>,

    /// Catalog publishing settings (README + logo → `__ocx.desc`).
    /// When omitted, defaults apply: `readme: CATALOG.md`, logo probed.
    #[serde(default)]
    pub catalog: Option<CatalogConfig>,

    /// OCI annotations written onto the image index of every published tag,
    /// on top of the ones auto-detected from the CI environment
    /// (`org.opencontainers.image.source` and `.revision`). A key given here
    /// overrides the auto-detected value for that key.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,

    /// Opt out of the generated drift-guard workflow (discouraged).
    ///
    /// When `false` (the default), `generate ci` also emits
    /// `.github/workflows/verify-generated.yml` — a CI job that re-renders from
    /// `mirror.yml` and fails if any generated workflow has been hand-edited.
    /// Set to `true` only when the repository deliberately maintains its
    /// workflows by hand; the drift guard is then not emitted and manual edits
    /// go unchecked.
    #[serde(default)]
    pub allow_manual_edits: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BuildTimestampFormat {
    Datetime,
    Date,
    None,
}

fn default_build_timestamp() -> BuildTimestampFormat {
    BuildTimestampFormat::Datetime
}

fn default_true() -> bool {
    true
}

fn default_wheel_scope() -> String {
    "pip-packages".to_string()
}

/// Regex for valid variant names: starts with lowercase letter, then lowercase
/// letters, digits, or dots.
static VARIANT_NAME_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z][a-z0-9.]*$").unwrap());

/// Regex for valid test entry names: starts with letter, then letters/digits/hyphens/underscores.
static TEST_NAME_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-zA-Z][a-zA-Z0-9_-]*$").unwrap());

/// Regex for a 40-character lowercase hexadecimal git SHA.
static GIT_REV_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[0-9a-f]{40}$").unwrap());

/// Regex for valid GitHub Actions secret names: `^[A-Z][A-Z0-9_]+$`.
///
/// Requires at least one uppercase letter, then one or more uppercase letters, digits, or
/// underscores. Names starting with `_` or containing only a single character are rejected
/// (GHA enforces both constraints in practice).
static GHA_SECRET_NAME_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[A-Z][A-Z0-9_]+$").unwrap());

/// Regex for a logical index package: `<namespace>/<package>`, each segment
/// lowercase alphanumeric with interior `.`, `_` or `-`.
static INDEX_PACKAGE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*$").unwrap());

/// Regex for a GitHub repository slug: `<owner>/<repo>`.
static GITHUB_REPO_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*/[A-Za-z0-9][A-Za-z0-9._-]*$").unwrap());

/// Regex for a Discord user ID (snowflake): 17–20 ASCII digits.
static DISCORD_USER_ID_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[0-9]{17,20}$").unwrap());

/// Regex for the characters a cron expression may contain: digits, the
/// day/month names GitHub accepts, and the `* / , -` operators.
static CRON_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[0-9A-Za-z*/,\- ]+$").unwrap());

impl MirrorSpec {
    pub fn validate(&self, spec_path: &Path) -> Vec<String> {
        let mut errors = Vec::new();
        let spec_dir = spec_path.parent().unwrap_or(Path::new("."));

        self.source.validate(&mut errors);

        // Env-package sources (`pylock`/`pypi`) and archive sources
        // (`github_release`/`url_index`) have disjoint content surfaces:
        // `wheels:` + `python:` on one side, `assets:`/`variants:`/`metadata:`/
        // `bin_scan:` on the other. `env_type` is `Some(<source.type>)` on the
        // env side and names the concrete type in every rejection message.
        let env_type = self.source.env_type_name();
        self.validate_assets_or_variants(env_type, spec_dir, &mut errors);

        if let Some(python) = &self.python {
            python.validate(&mut errors);
            // A `pylock` source already resolves its own lock from the
            // committed file; `python.lock` only configures *derivation* of a
            // lock, which is meaningless without something to derive it from.
            if python.lock.is_some() && matches!(self.source, Source::Pylock { .. }) {
                errors.push(
                    "python.lock: only supported for source.type 'pypi' (a committed lock is already resolved)"
                        .to_string(),
                );
            }
        } else if let Some(source_type) = env_type {
            errors.push(format!("python: required for source.type '{source_type}'"));
        }

        match (&self.metadata, env_type) {
            (Some(_), Some(source_type)) => errors.push(metadata_not_supported_error(source_type)),
            (Some(metadata), None) => metadata.validate(spec_dir, &mut errors),
            (None, _) => {}
        }

        // `bin_scan` derives the `binaries` claim from the extracted archive
        // tree. An env package has no archive — its tree is composed from
        // wheels, and its interface comes from the lock — so a scan mode here
        // could never be honoured. Rejected outright like `metadata:` rather
        // than silently ignored. `libc_lint` is *not* rejected but is inert
        // for env specs (no extracted tree, private-visibility PATH vars —
        // see `pipeline/python_prepare.rs`); accepting it keeps a shared
        // `extends:` base usable by both source kinds.
        if let Some(source_type) = env_type
            && self.bin_scan.scans()
        {
            errors.push(bin_scan_not_supported_error(source_type));
        }

        // A `bin_scan` is the one metadata setting whose misconfiguration
        // publishes a wrong claim instead of failing, so it is checked here,
        // before anything can push. Per effective variant, because a
        // variant-level `bin_scan` overrides the spec-level one — checking only
        // `self.bin_scan` would let a scanning variant through a spec whose top
        // level is `off`. Guarded because `effective_variants` assumes the
        // assets-xor-variants rule this same run may still be reporting.
        if self.assets.is_some() || self.variants.is_some() {
            for variant in self.effective_variants() {
                let Some(config) = &variant.metadata else { continue };
                if !variant.bin_scan.scans() {
                    continue;
                }
                let label = match &variant.name {
                    Some(name) => format!("variants.{name}.bin_scan"),
                    None => "bin_scan".to_string(),
                };
                config.validate_scannable(spec_dir, &label, variant.bin_scan, &mut errors);
            }
        }

        self.cascade.validate(&mut errors);

        if let Some(versions) = &self.versions {
            versions.validate(&mut errors);
        }

        if let Some(tests) = &self.tests {
            validate_tests(tests, &mut errors);
        }
        if let Some(platforms) = &self.platforms {
            validate_platforms(platforms, &mut errors);
        }
        if let Some(ocx_mirror) = &self.ocx_mirror {
            validate_ocx_mirror_config(ocx_mirror, &mut errors);
        }
        if let Some(notify) = &self.notify {
            validate_notify_config(notify, &mut errors);
        }
        if let Some(announce) = &self.announce {
            validate_announce_config(announce, &mut errors);
        }
        crate::annotations::validate(&self.annotations, &mut errors);

        errors
    }

    /// Whether this spec publishes cascade tags without a build timestamp.
    ///
    /// With `cascade: true` and `build_timestamp: none`, every re-publish of a
    /// version re-points the bare `X.Y.Z` tag (and the rolling `X.Y` / `X` /
    /// `latest` tags) to a fresh digest. The prior digest is then only reachable
    /// by content address, so once the registry runs garbage collection it can be
    /// reaped — breaking any consumer `ocx.lock` pinned to that `@sha256:` digest.
    /// A non-`none` `build_timestamp` keeps each build under a unique, permanently
    /// retained `X.Y.Z_<ts>` tag, so the digest never fully orphans (issue #12).
    ///
    /// This is an advisory hazard, not a hard error: registry-side retention or a
    /// referrers policy can make `none` safe, so `load_spec` warns rather than rejects.
    fn cascade_without_build_stamp(&self) -> bool {
        self.cascade.enabled && self.build_timestamp == BuildTimestampFormat::None
    }

    /// Whether `platform` applies to `version` under the per-platform
    /// applicability rules declared on `platforms.<platform>`.
    ///
    /// Returns `false` when `version` is below the platform's inclusive
    /// `min_version`, at or above its exclusive `max_version`, or matched by any
    /// `exclude` entry (single version or range). An undeclared platform — or a
    /// platform with no bounds and no excludes — applies to every version.
    ///
    /// Build metadata on `version` (the mirror's per-run timestamp suffix) is
    /// stripped before comparison, so applicability is decided on the release
    /// core (`X.Y.Z[-pre]`) regardless of the build stamp or variant prefix.
    pub fn platform_applies(&self, version: &str, platform: &str) -> bool {
        let Some(config) = self.platforms.as_ref().and_then(|p| p.get(platform)) else {
            return true;
        };
        let Some(parsed) = Version::parse(version).map(|v| applicability_key(&v)) else {
            // Unparseable versions are kept — consistent with `filter.rs` bounds.
            return true;
        };

        if let Some(min) = config.min_version.as_ref().and_then(|s| Version::parse(s))
            && parsed < min
        {
            return false;
        }
        if let Some(max) = config.max_version.as_ref().and_then(|s| Version::parse(s))
            && parsed >= max
        {
            return false;
        }
        !config.exclude.iter().any(|entry| entry.matches(&parsed))
    }

    /// Returns the `exclude` entry matching `(version, platform)`, if any.
    ///
    /// Used for visibility (the 🔒 row in the Discord report): the matched entry
    /// carries the `severity` and optional `reason`. Build metadata and any
    /// variant prefix on `version` are stripped before matching, mirroring
    /// [`platform_applies`].
    ///
    /// [`platform_applies`]: Self::platform_applies
    pub fn exclude_hit(&self, version: &str, platform: &str) -> Option<&ExcludeEntry> {
        let config = self.platforms.as_ref()?.get(platform)?;
        let parsed = Version::parse(version).map(|v| applicability_key(&v))?;
        config.exclude.iter().find(|entry| entry.matches(&parsed))
    }

    /// Validate the `assets` / `variants` / `wheels` surface, source-aware.
    ///
    /// `github_release` / `url_index` sources resolve assets via regex
    /// patterns — exactly one of top-level `assets` xor per-variant `assets`
    /// must be present. Env-package sources (`pylock`/`pypi`, `env_type` =
    /// `Some`) select wheels via the per-platform `wheels:` map instead;
    /// `assets` and `variants` on such a spec are meaningless and rejected
    /// outright rather than silently ignored (libc is a platform `os.features`
    /// axis for env packages, not a variant axis).
    fn validate_assets_or_variants(&self, env_type: Option<&str>, spec_dir: &Path, errors: &mut Vec<String>) {
        if let Some(source_type) = env_type {
            if self.assets.is_some() {
                errors.push(format!(
                    "assets: not supported for source.type '{source_type}' (use the per-platform 'wheels' map instead)"
                ));
            }
            if self.variants.is_some() {
                errors.push(format!(
                    "variants: not supported for source.type '{source_type}' \
                     (declare '+libc.glibc'/'+libc.musl' wheels keys instead)"
                ));
            }
            match &self.wheels {
                Some(wheels) => {
                    if wheels.filters.is_empty() {
                        errors.push("wheels: must declare at least one platform key".to_string());
                    }
                    wheels.validate(errors);
                    self.validate_wheels_platform_coverage(wheels, errors);
                }
                None => errors.push(format!(
                    "wheels: required for source.type '{source_type}' (per-platform wheel filters)"
                )),
            }
            return;
        }

        if self.wheels.is_some() {
            errors.push("wheels: only supported for source.type 'pylock'/'pypi'".to_string());
        }

        // Validate assets/variants mutual exclusivity
        match (&self.assets, &self.variants) {
            (Some(_), Some(_)) => {
                errors.push("cannot specify both top-level 'assets' and 'variants'".to_string());
            }
            (None, None) => {
                errors.push("must specify either 'assets' or 'variants'".to_string());
            }
            (Some(assets), None) => {
                assets.validate(errors);
            }
            (None, Some(variants)) => {
                self.validate_variants(variants, spec_dir, errors);
            }
        }
    }

    /// Cross-validate `wheels:` keys against the `platforms:` CI matrix: every
    /// wheels key needs a test leg (its base os/arch declared under
    /// `platforms:`), and every declared platform leg must have at least one
    /// wheels key to test — an uncovered leg would fail closed at push time
    /// anyway, so reject it up front.
    fn validate_wheels_platform_coverage(&self, wheels: &WheelPatterns, errors: &mut Vec<String>) {
        let platform_keys: HashSet<&str> = self
            .platforms
            .as_ref()
            .map(|platforms| platforms.keys().map(String::as_str).collect())
            .unwrap_or_default();

        let mut covered: HashSet<String> = HashSet::new();
        for platform in wheels.filters.keys() {
            let base = base_platform_key(platform);
            if !platform_keys.is_empty() && !platform_keys.contains(base.as_str()) {
                errors.push(format!(
                    "wheels.{platform}: base platform '{base}' is not declared under 'platforms'"
                ));
            }
            covered.insert(base);
        }
        for key in &platform_keys {
            if !covered.contains(*key) {
                errors.push(format!("platforms.{key}: no wheels key covers this platform"));
            }
        }
    }

    fn validate_variants(&self, variants: &[VariantSpec], spec_dir: &Path, errors: &mut Vec<String>) {
        if variants.is_empty() {
            errors.push("variants: must declare at least one variant".to_string());
            return;
        }

        let default_count = variants.iter().filter(|v| v.default).count();
        if default_count != 1 {
            errors.push(format!(
                "variants: exactly one variant must be default, found {default_count}"
            ));
        }

        let mut seen_names: HashSet<Option<&String>> = HashSet::new();
        for v in variants {
            match &v.name {
                Some(name) => {
                    // Name format
                    if !VARIANT_NAME_RE.is_match(name) {
                        errors.push(format!("variants: invalid name '{name}' (must match [a-z][a-z0-9.]*)",));
                    }

                    // Reserved name
                    if name == "latest" {
                        errors.push("variants: 'latest' is reserved and cannot be used as a variant name".to_string());
                    }
                }
                None => {
                    // Unnamed variant must be the default
                    if !v.default {
                        errors.push("variants: unnamed variant must be the default".to_string());
                    }
                }
            }

            // Duplicate check (None counts as a unique entry)
            if !seen_names.insert(v.name.as_ref()) {
                match &v.name {
                    Some(name) => errors.push(format!("variants: duplicate name '{name}'")),
                    None => errors.push("variants: duplicate unnamed variant".to_string()),
                }
            }

            // Per-variant asset validation
            v.assets.validate(errors);

            // Per-variant metadata validation
            if let Some(metadata) = &v.metadata {
                metadata.validate(spec_dir, errors);
            }
        }
    }

    /// Returns the effective variant list, handling backward compatibility.
    ///
    /// - No `variants` key: single synthetic variant using top-level fields.
    /// - With `variants` key: one [`EffectiveVariant`] per declared variant,
    ///   inheriting top-level `metadata` and `asset_type` as defaults.
    pub fn effective_variants(&self) -> Vec<EffectiveVariant> {
        match &self.variants {
            Some(variants) => variants
                .iter()
                .map(|v| EffectiveVariant {
                    name: v.name.clone(),
                    is_default: v.default,
                    assets: v.assets.clone(),
                    metadata: v.metadata.clone().or_else(|| self.metadata.clone()),
                    asset_type: v.asset_type.clone().or_else(|| self.asset_type.clone()),
                    bin_scan: v.bin_scan.unwrap_or(self.bin_scan),
                    libc_lint: v.libc_lint.unwrap_or(self.libc_lint),
                })
                .collect(),
            None => vec![EffectiveVariant {
                name: None,
                is_default: true,
                assets: self
                    .assets
                    .clone()
                    .expect("validated: assets or variants must be present"),
                metadata: self.metadata.clone(),
                asset_type: self.asset_type.clone(),
                bin_scan: self.bin_scan,
                libc_lint: self.libc_lint,
            }],
        }
    }
}

// ── Pipeline field validators ────────────────────────────────────────────────

/// Strip build metadata (the mirror's per-run timestamp suffix) from a version
/// so applicability decisions compare the release core only.
///
/// `parent()` removes the innermost component; when `has_build()` is true that
/// component is exactly the build segment (it implies `has_patch()`, so
/// `parent()` is always `Some`).
fn strip_build(version: &Version) -> Version {
    if version.has_build() {
        version.parent().unwrap_or_else(|| version.clone())
    } else {
        version.clone()
    }
}

/// Reduce a version to its applicability key: strip the build-metadata stamp
/// ([`strip_build`]) and any variant prefix, so applicability and exclusion
/// decisions compare on the release core (`X.Y.Z[-pre]`) regardless of build
/// stamp or variant. Variants are orthogonal to platform applicability — a
/// variant build of `X.Y.Z` (e.g. `debug-X.Y.Z`) is still `X.Y.Z` for window
/// and exclude matching, which the push pipeline keys off the variant-prefixed
/// tag.
fn applicability_key(version: &Version) -> Version {
    strip_build(version).without_variant()
}

#[cfg(test)]
#[path = "spec/tests.rs"]
mod tests;
