// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

mod announce_config;
mod asset_type;
mod assets;
mod bin_scan;
mod cascade_config;
mod catalog_config;
mod concurrency_config;
mod metadata_config;
mod notify_config;
mod ocx_mirror_config;
mod platforms_config;
mod python_config;
mod source;
mod strip_components_config;
mod target;
mod tests_config;
mod variant;
mod verify_config;
mod versions_config;
mod wheels;

#[allow(unused_imports)]
pub use announce_config::{AnnounceConfig, DEFAULT_INDEX_REPO};
pub use asset_type::{AssetType, AssetTypeConfig};
pub use assets::AssetPatterns;
pub use bin_scan::BinScanMode;
pub use cascade_config::CascadeConfig;
pub use catalog_config::CatalogConfig;
pub use concurrency_config::{ConcurrencyConfig, resolve_compression_threads};
pub use metadata_config::MetadataConfig;
#[allow(unused_imports)]
pub use notify_config::{DiscordConfig, NotifyConfig};
pub use ocx_mirror_config::OcxMirrorConfig;
#[allow(unused_imports)]
pub use platforms_config::{ContainerConfig, ExcludeEntry, PlatformConfig, Severity};
pub use python_config::{LockOptions, PythonConfig};
pub use source::{GeneratorConfig, Source, UrlIndexSource, UrlIndexVersion};
pub use strip_components_config::StripComponentsConfig;
pub use target::Target;
pub use tests_config::{TestEntry, TestKind};
pub use variant::{EffectiveVariant, VariantSpec};
pub use verify_config::VerifyConfig;
pub(crate) use versions_config::BackfillOrder;
pub use versions_config::VersionsConfig;
pub use wheels::{WheelPatterns, base_platform_key, libc_feature};

use ocx_lib::log;
use ocx_lib::oci::Platform;
use ocx_lib::package::version::Version;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::error::MirrorError;

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

/// Reject a cron expression that cannot be interpolated into a generated
/// workflow's `schedule:` block.
///
/// GitHub remains the only validator of cron *semantics* — a nonsense but
/// well-formed `99 99 * * *` still renders. The charset guard exists because
/// the value is spliced verbatim into `on:` inside a single-quoted scalar
/// (`schedule_block` in `generate/ci.rs`): a quote or newline would close that
/// scalar and let a spec add triggers of its own, and a scheduled cascade run
/// repairs for real.
pub(crate) fn validate_cron(label: &str, cron: &str, errors: &mut Vec<String>) {
    if cron.trim().is_empty() || !CRON_RE.is_match(cron) {
        errors.push(format!("{label}: invalid cron expression '{cron}'"));
    }
}

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

/// The `metadata:` rejection message for an env source (`pylock`/`pypi`):
/// env metadata is composed from the resolved lock, so a hand-authored
/// `metadata.json` has nothing to attach to.
fn metadata_not_supported_error(source_type: &str) -> String {
    format!(
        "metadata: not supported for source.type '{source_type}' \
         (env metadata is composed from the lock; use catalog:/CATALOG.md for the description)"
    )
}

/// The `bin_scan:` rejection message for an env source, shaped like
/// [`metadata_not_supported_error`]: both name a setting that only an
/// extracted archive tree could satisfy.
fn bin_scan_not_supported_error(source_type: &str) -> String {
    format!(
        "bin_scan: not supported for source.type '{source_type}' \
         (an env package has no extracted archive to scan; its interface comes from the lock)"
    )
}

// ── Pipeline field validators ────────────────────────────────────────────────

/// Validate `tests:` entries: non-empty, unique names, valid name regex,
/// and exactly one of `command|script|script_inline` set per entry.
fn validate_tests(tests: &[TestEntry], errors: &mut Vec<String>) {
    if tests.is_empty() {
        errors.push("tests: must contain at least one entry".to_string());
        return;
    }

    let mut seen = HashSet::new();
    for entry in tests {
        if !TEST_NAME_RE.is_match(&entry.name) {
            errors.push(format!(
                "tests: invalid name '{}' (must match ^[a-zA-Z][a-zA-Z0-9_-]*$)",
                entry.name
            ));
        }
        if !seen.insert(&entry.name) {
            errors.push(format!("tests: duplicate name '{}'", entry.name));
        }

        // Exactly-one-of enforcement.
        let set_count = [
            entry.command.is_some(),
            entry.script.is_some(),
            entry.script_inline.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        match set_count {
            1 => {}
            0 => errors.push(format!(
                "tests: entry '{}' must set exactly one of command|script|script_inline (none set)",
                entry.name
            )),
            n => errors.push(format!(
                "tests: entry '{}' must set exactly one of command|script|script_inline ({n} set)",
                entry.name
            )),
        }
    }
}

/// Check that every `script:` a spec names exists, resolved from the repository
/// root.
///
/// `script:` is the one spec path that is repository-root-relative:
/// `metadata.default` and `catalog.*` resolve against the spec's own directory,
/// but the generated workflow runs `ocx package test --script` from the checkout
/// root. In a single-spec repository those are the same directory and the
/// asymmetry never shows; in a multi-spec one they diverge, and the natural
/// `script: tests/smoke.star` written inside `buildifier/mirror.yml` quietly
/// means `<repo>/tests/smoke.star`. A path that resolves to nothing renders
/// green here and fails as a red test leg after a publish attempt, so it is a
/// spec error (exit 65) like a missing `metadata.default`.
///
/// `spec_dir` is the spec's own directory *relative to `repo_root`* — `None` for
/// a root spec, where the near miss cannot arise.
pub(crate) fn validate_test_scripts(spec: &MirrorSpec, repo_root: &Path, spec_dir: Option<&Path>) -> Vec<String> {
    let mut errors = Vec::new();
    check_test_scripts("tests", spec.tests.as_deref(), repo_root, spec_dir, &mut errors);

    // Per-platform overrides carry scripts too, and nothing else validates them.
    if let Some(platforms) = &spec.platforms {
        let mut keys: Vec<&String> = platforms.keys().collect();
        keys.sort();
        for key in keys {
            check_test_scripts(
                &format!("platforms: '{key}': tests"),
                platforms[key].tests.as_deref(),
                repo_root,
                spec_dir,
                &mut errors,
            );
        }
    }
    errors
}

fn check_test_scripts(
    scope: &str,
    entries: Option<&[TestEntry]>,
    repo_root: &Path,
    spec_dir: Option<&Path>,
    errors: &mut Vec<String>,
) {
    for entry in entries.unwrap_or_default() {
        let Some(script) = &entry.script else { continue };
        let resolved = repo_root.join(script);
        if resolved.exists() {
            continue;
        }
        // Name what was resolved and against what — the author wrote a path that
        // looks right from where they were standing.
        let mut message = format!(
            "{scope}: entry '{}' script not found: {} resolves from the repository root as {}",
            entry.name,
            script.display(),
            resolved.display(),
        );
        // The near miss is the actual mistake being made, so say it outright
        // rather than leave the author to discover the asymmetry.
        if let Some(dir) = spec_dir
            && repo_root.join(dir).join(script).exists()
        {
            message.push_str(&format!(
                " — `script:` is repository-root-relative, unlike metadata.default and catalog.*, \
                 which resolve from the spec's directory; write {}",
                dir.join(script).display(),
            ));
        }
        errors.push(message);
    }
}

/// The repository basename of a container image, with the registry prefix and
/// the tag stripped (`docker.io/library/alpine:3.20` → `alpine`).
///
/// Every distro-family inference keys off this one spelling, so a
/// registry-qualified image classifies the same way its bare form does.
fn image_basename(image: &str) -> &str {
    // Strip the tag (everything after `:`), then take the last path component.
    let image_name = image.split(':').next().unwrap_or(image);
    image_name.split('/').next_back().unwrap_or(image_name)
}

/// Infer the default shell for a container image based on its image-name prefix.
///
/// Returns `Some(shell)` when a well-known distro prefix matches, `None` when
/// the image is non-standard and an explicit `shell` is required.
pub(crate) fn infer_shell_from_image(image: &str) -> Option<&'static str> {
    let base = image_basename(image);

    // Well-known distros that default to bash.
    const BASH_PREFIXES: &[&str] = &["ubuntu", "debian", "fedora", "rocky", "opensuse"];
    // Alpine defaults to sh (no bash by default).
    const SH_PREFIXES: &[&str] = &["alpine"];

    for prefix in BASH_PREFIXES {
        if base.starts_with(prefix) {
            return Some("bash");
        }
    }
    for prefix in SH_PREFIXES {
        if base.starts_with(prefix) {
            return Some("sh");
        }
    }

    None
}

/// The libc family a container image's userland links against, which selects
/// the statically-linked `ocx` release a container test leg mounts.
///
/// Alpine is musl; every other supported base (Debian, Ubuntu, Fedora, Rocky,
/// openSUSE) is gnu. Running a gnu-linked `ocx` on Alpine fails with a bare
/// "not found" from the loader, so this is the difference between a leg that
/// tests the artifact and one that cannot start.
///
// ponytail: name-prefix inference, not a spec field — the corpus needs exactly
// alpine(musl) + the glibc distros. Add an explicit `containers[].libc` to
// `ContainerConfig` when a musl image that is not Alpine shows up.
pub(crate) fn infer_libc_from_image(image: &str) -> &'static str {
    if image_basename(image).starts_with("alpine") {
        "musl"
    } else {
        "gnu"
    }
}

/// The `os.features` value that declares a given libc family.
///
/// The rust triple spells glibc `gnu`; the OCI feature spells it `libc.glibc`.
/// Crossing the two names is the whole point of the cross-check, so the
/// translation lives in one place.
///
/// Distinct from [`libc_feature`](wheels::libc_feature), which reads a feature
/// back off a platform key: this one goes the other way, from an inferred
/// family name to the feature that would declare it.
fn libc_family_feature(family: &str) -> &'static str {
    if family == "musl" { "libc.musl" } else { "libc.glibc" }
}

/// The on-disk / artifact-name slug for a platform.
///
/// `linux/amd64` → `linux_amd64`; `linux/amd64+libc.musl` → `linux_amd64_libc.musl`.
///
/// This is the second join key of the pipeline (after
/// [`image_to_container_id`]): `pipeline prepare` names its work directory with
/// it, the CI workflow flattens that directory into `bundle-{V}-{slug}.tar.xz`
/// and `junit-{V}-{slug}-{container_id}.xml`, and `pipeline push` looks both
/// back up by it. `ascii_segments` drops `os_features`, so two platforms
/// differing only by libc would collide — the sorted, deduped feature suffix is
/// what keeps them apart. Every producer and consumer must call this one
/// function or a libc-bearing platform's artifacts become invisible downstream.
pub(crate) fn platform_slug(platform: &Platform) -> String {
    use ocx_lib::utility::string_ext::StringExt as _;

    let mut slug = platform.ascii_segments().join("_");

    if let Platform::Specific { os_features, .. } = platform
        && !os_features.is_empty()
    {
        let mut sorted = os_features.clone();
        sorted.sort();
        sorted.dedup();
        slug.push('_');
        slug.push_str(&sorted.join("_").to_relaxed_slug());
    }

    slug
}

/// [`platform_slug`] for a spec platform key in string form.
///
/// An unparseable key falls back to the naive `/` → `_` replacement; validation
/// rejects such keys, so the fallback only keeps callers total.
pub(crate) fn platform_key_slug(key: &str) -> String {
    key.parse::<Platform>()
        .map(|p| platform_slug(&p))
        .unwrap_or_else(|_| key.replace('/', "_"))
}

/// A spec platform key stripped of its `os.features` suffix.
///
/// `docker run --platform` speaks OCI `os/arch[/variant]` and rejects the
/// `+libc.musl` suffix outright, while the matrix label, the `--platform` flag
/// of `ocx package test` and the `discover` platform set all need the full key.
/// Strip only where docker looks.
pub(crate) fn platform_without_features(key: &str) -> String {
    key.parse::<Platform>()
        .map(|p| p.segments().join("/"))
        .unwrap_or_else(|_| key.to_string())
}

/// Slugify a container image reference into a JUnit-filename `container_id`.
///
/// All `:`, `/` and `.` separators become `_`, and consecutive underscores
/// collapse — e.g. `ubuntu:24.04` → `ubuntu_24_04`, `alpine:3.20` → `alpine_3_20`.
///
/// This is the join key between the two halves of a container run: the CI
/// renderer names each leg's JUnit file with it, and `pipeline push` looks the
/// file back up by it. The two must agree exactly or every container leg's
/// result reads as missing and nothing is ever published — so both call this.
pub(crate) fn image_to_container_id(image: &str) -> String {
    image.replace([':', '/', '.'], "_").replace("__", "_")
}

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

/// Validate a single `exclude` entry: exactly one of single-`version` or a
/// `min_version`/`max_version` range, and any present version parses.
fn validate_exclude_entry(key: &str, index: usize, entry: &ExcludeEntry, errors: &mut Vec<String>) {
    let has_version = entry.version.is_some();
    let has_range = entry.min_version.is_some() || entry.max_version.is_some();

    if !has_version && !has_range {
        errors.push(format!(
            "platforms: '{key}': exclude[{index}] must set 'version' or a 'min_version'/'max_version' range"
        ));
    }
    if has_version && has_range {
        errors.push(format!(
            "platforms: '{key}': exclude[{index}] cannot set both 'version' and a 'min_version'/'max_version' range"
        ));
    }
    for (field, value) in [
        ("version", &entry.version),
        ("min_version", &entry.min_version),
        ("max_version", &entry.max_version),
    ] {
        if let Some(raw) = value {
            match Version::parse(raw) {
                None => errors.push(format!(
                    "platforms: '{key}': exclude[{index}] {field} '{raw}' is not a valid version"
                )),
                // Match keys on the release core, so a variant/build-stamped
                // bound would compare asymmetrically — require a plain version.
                Some(parsed) if applicability_key(&parsed) != parsed => errors.push(format!(
                    "platforms: '{key}': exclude[{index}] {field} '{raw}' must be a plain version without a variant prefix or build metadata"
                )),
                Some(_) => {}
            }
        }
    }
    // An inverted exclude range (min ≥ max) matches nothing — a silent no-op. Reject it.
    if let Some(min_raw) = entry.min_version.as_ref()
        && let Some(max_raw) = entry.max_version.as_ref()
        && let Some(min) = Version::parse(min_raw)
        && let Some(max) = Version::parse(max_raw)
        && applicability_key(&min) >= applicability_key(&max)
    {
        errors.push(format!(
            "platforms: '{key}': exclude[{index}] min_version '{min_raw}' must be below max_version '{max_raw}'"
        ));
    }
}

/// Validate one container's `setup:` list.
///
/// Each entry becomes a single Dockerfile `RUN`, passed to the container's
/// shell as written. Rejected here are the shapes that would not arrive as one
/// command: a list that declares nothing to run, an entry that runs nothing, an
/// entry carrying a newline (the natural `script_inline`-style mistake, which
/// splits one `RUN` into a broken Dockerfile), and an entry ending in a
/// backslash — a line continuation, which quietly absorbs the *next* `RUN` and
/// leaves that layer unbuilt while the build still exits 0.
fn validate_container_setup(key: &str, container: &ContainerConfig, errors: &mut Vec<String>) {
    let Some(setup) = &container.setup else {
        return;
    };
    let image = &container.image;
    if setup.is_empty() {
        errors.push(format!(
            "platforms: '{key}': container image '{image}' declares an empty setup list; \
             drop the key or give it at least one command"
        ));
    }
    for (index, command) in setup.iter().enumerate() {
        if command.trim().is_empty() {
            errors.push(format!(
                "platforms: '{key}': container image '{image}': setup[{index}] must not be blank"
            ));
        } else if command.contains('\n') {
            errors.push(format!(
                "platforms: '{key}': container image '{image}': setup[{index}] must be a single \
                 command (each entry becomes one Dockerfile RUN); split it across entries"
            ));
        } else if command.trim_end().ends_with('\\') {
            // Trimmed first: docker continues the line on a backslash that is
            // the last *non-whitespace* character, so trailing spaces do not
            // save it. Either way `RUN foo \` swallows the next `RUN` as its
            // own arguments — the build exits 0 having skipped that layer,
            // leaving the leg green on an unprovisioned image.
            errors.push(format!(
                "platforms: '{key}': container image '{image}': setup[{index}] must not end with \
                 a backslash; it would continue into the next RUN instead of ending the command"
            ));
        }
    }
}

/// Validate `platforms:` map: valid platform keys, runner present, container
/// image format, shell defaults for known distros, explicit shell required for
/// unknown, per-container `setup:` commands, plus per-platform version
/// applicability (`min_version`, `max_version`, `exclude`).
fn validate_platforms(platforms: &HashMap<String, PlatformConfig>, errors: &mut Vec<String>) {
    for (key, config) in platforms {
        // The canonical `os/arch[/variant][+feature,…]` grammar, parsed by the
        // same `FromStr` the `assets:` keys and every `--platform` flag use. A
        // hand-rolled regex here is what kept `linux/amd64+libc.musl` — the only
        // way to declare a libc claim — out of the test matrix entirely.
        let parsed = key.parse::<Platform>();
        if parsed.is_err() {
            errors.push(format!(
                "platforms: invalid key '{key}' (must be os/arch[+feature] format, \
                 e.g. linux/amd64 or linux/amd64+libc.musl)"
            ));
        }

        if config.runner.trim().is_empty() {
            errors.push(format!("platforms: '{key}': runner must not be empty"));
        }

        for (field, value) in [
            ("min_version", &config.min_version),
            ("max_version", &config.max_version),
        ] {
            if let Some(raw) = value {
                match Version::parse(raw) {
                    None => errors
                        .push(format!("platforms: '{key}': {field} '{raw}' is not a valid version")),
                    // Applicability compares on the release core (build stamp and
                    // variant prefix stripped via `applicability_key`); a bound
                    // carrying either would compare asymmetrically and silently
                    // misfilter, so require a plain version here.
                    Some(parsed) if applicability_key(&parsed) != parsed => errors.push(format!(
                        "platforms: '{key}': {field} '{raw}' must be a plain version without a variant prefix or build metadata"
                    )),
                    Some(_) => {}
                }
            }
        }
        // An inverted window (min ≥ max) silently drops the platform from every
        // version. Reject it — min is inclusive, max exclusive, so equal is empty too.
        if let Some(min_raw) = config.min_version.as_ref()
            && let Some(max_raw) = config.max_version.as_ref()
            && let Some(min) = Version::parse(min_raw)
            && let Some(max) = Version::parse(max_raw)
            && applicability_key(&min) >= applicability_key(&max)
        {
            errors.push(format!(
                "platforms: '{key}': min_version '{min_raw}' must be below max_version '{max_raw}'"
            ));
        }
        for (index, entry) in config.exclude.iter().enumerate() {
            validate_exclude_entry(key, index, entry, errors);
        }

        if let Some(containers) = &config.containers {
            if containers.is_empty() {
                errors.push(format!(
                    "platforms: '{key}': containers must contain at least one entry when declared"
                ));
            } else {
                // Container legs are `docker run --platform <key>` on a Linux
                // runner. A macOS or Windows runner has no Linux container
                // engine at all, so the pairing can only ever fail at run time —
                // reject it while the maintainer is still looking at the spec.
                if !key.starts_with("linux/") {
                    errors.push(format!(
                        "platforms: '{key}': containers are linux-only (tests run via `docker run`)"
                    ));
                }
                // The libc family the platform key claims, if it claims one.
                // Declaring `+libc.musl` and then testing in a glibc image is
                // the silent failure this whole matrix exists to prevent: a
                // musl-static artifact runs fine under glibc, so the leg goes
                // green having verified nothing. Reject the pairing here, where
                // the maintainer is still looking at the spec.
                let declared_libc: Vec<&str> = match parsed.as_ref() {
                    Ok(Platform::Specific { os_features, .. }) => os_features
                        .iter()
                        .filter(|f| f.starts_with("libc."))
                        .map(String::as_str)
                        .collect(),
                    _ => Vec::new(),
                };

                for container in containers {
                    // If no explicit shell, the image must have a known default.
                    if container.shell.is_none() && infer_shell_from_image(&container.image).is_none() {
                        errors.push(format!(
                            "platforms: '{key}': container image '{}' has ambiguous shell; \
                             set an explicit shell (e.g. shell: bash)",
                            container.image
                        ));
                    }

                    let image_libc = libc_family_feature(infer_libc_from_image(&container.image));
                    if !declared_libc.is_empty() && !declared_libc.contains(&image_libc) {
                        errors.push(format!(
                            "platforms: '{key}': container image '{}' is {image_libc}, \
                             but the platform declares {} — the leg would run without \
                             testing the libc claim",
                            container.image,
                            declared_libc.join(",")
                        ));
                    }

                    validate_container_setup(key, container, errors);
                }
            }
        }
    }
}

/// Validate `ocx_mirror:` block: rev format.
fn validate_ocx_mirror_config(config: &OcxMirrorConfig, errors: &mut Vec<String>) {
    if let Some(rev) = &config.rev
        && !GIT_REV_RE.is_match(rev)
    {
        errors.push(format!(
            "ocx_mirror: rev '{rev}' must be a 40-character lowercase hex SHA"
        ));
    }
}

/// Content-policy check on the `notify:` block.
///
/// Rejects any `webhook_secret` value that looks like a hardcoded URL. This is a
/// *policy* violation (exit 64 / `SpecUsageError`), distinct from the structural
/// format check in `validate_notify_config` (exit 65 / `SpecInvalid`).
///
/// Call this from `load_spec` **before** `spec.validate()` so the correct exit code
/// is returned even when a structurally-valid spec contains a bad policy choice.
pub(crate) fn policy_check_notify(notify: &NotifyConfig) -> Result<(), MirrorError> {
    let Some(discord) = &notify.discord else {
        return Ok(());
    };
    let secret = &discord.webhook_secret;

    // R3 mitigation: reject any hardcoded URL — catches accidental paste of the raw webhook URL.
    if secret.starts_with("https://") || secret.starts_with("http://") {
        return Err(MirrorError::SpecUsageError(format!(
            "webhook_secret: hardcoded URL not allowed; use a GitHub Actions secret name instead (got '{secret}')"
        )));
    }
    if secret.contains("discord.com") || secret.contains("discordapp.com") {
        return Err(MirrorError::SpecUsageError(format!(
            "webhook_secret: value must not contain a Discord URL; use a GitHub Actions secret name instead (got '{secret}')"
        )));
    }

    // The user id is non-secret but a frequent paste mistake — catch a URL or
    // `@mention` early (exit 64) rather than letting it slip into the workflow.
    if let Some(user_id) = &discord.user_id {
        if user_id.starts_with("https://") || user_id.starts_with("http://") {
            return Err(MirrorError::SpecUsageError(format!(
                "notify.discord.user_id: hardcoded URL not allowed; use the numeric Discord user ID (got '{user_id}')"
            )));
        }
        if user_id.contains('@') {
            return Err(MirrorError::SpecUsageError(format!(
                "notify.discord.user_id: must be the numeric Discord snowflake, not an @mention (got '{user_id}')"
            )));
        }
    }

    Ok(())
}

/// Validate `notify:` block: webhook_secret must be a valid GHA secret name format.
///
/// URL-literal checks are handled separately by [`policy_check_notify`] with a
/// `SpecUsageError` (exit 64). This function only checks the structural format,
/// contributing to `SpecInvalid` (exit 65) errors.
fn validate_notify_config(config: &NotifyConfig, errors: &mut Vec<String>) {
    let Some(discord) = &config.discord else {
        return;
    };

    let secret = &discord.webhook_secret;

    // Must match GHA secret name format.
    if !GHA_SECRET_NAME_RE.is_match(secret) {
        errors.push(format!(
            "webhook_secret: '{secret}' is not a valid GitHub Actions secret name \
             (must match ^[A-Z][A-Z0-9_]+$)"
        ));
    }

    // The mention target must be a numeric Discord snowflake (17–20 digits).
    if let Some(user_id) = &discord.user_id
        && !DISCORD_USER_ID_RE.is_match(user_id)
    {
        errors.push(format!(
            "notify.discord.user_id: '{user_id}' is not a valid Discord user ID (must match ^[0-9]{{17,20}}$)"
        ));
    }
}

/// Validate the `announce:` block: the logical package and both repository
/// slugs must be well-formed `<a>/<b>` pairs, and the optional catch-up
/// schedule must be a cron expression safe to splice into a generated `on:`
/// block (see [`validate_cron`]).
///
/// A malformed value is reported as a named field error (contributing to
/// `SpecInvalid`, exit 65) rather than a serde shape mismatch, so the message
/// names the field and what it expected.
fn validate_announce_config(config: &AnnounceConfig, errors: &mut Vec<String>) {
    if let Some(cron) = &config.schedule {
        validate_cron("announce.schedule", cron, errors);
    }
    if !INDEX_PACKAGE_RE.is_match(&config.package) {
        errors.push(format!(
            "announce.package: '{}' is not a valid index package (must be '<namespace>/<package>', \
             lowercase alphanumeric with '.', '_' or '-')",
            config.package
        ));
    }
    for (field, value) in [("fork", &config.fork), ("index_repo", &config.index_repo)] {
        if !GITHUB_REPO_RE.is_match(value) {
            errors.push(format!(
                "announce.{field}: '{value}' is not a valid GitHub repository (must be '<owner>/<repo>')"
            ));
        }
    }
}

/// Load and validate a mirror spec from a YAML file, resolving `extends` chains.
///
/// If the spec contains an `extends` key, the referenced base file is loaded first
/// and the child's top-level keys are shallow-merged on top. Chains of arbitrary
/// depth are supported; circular references are detected and rejected.
pub async fn load_spec(spec_path: &Path) -> Result<MirrorSpec, MirrorError> {
    if !spec_path.exists() {
        return Err(MirrorError::SpecNotFound(spec_path.display().to_string()));
    }

    let content = tokio::fs::read_to_string(spec_path)
        .await
        .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", spec_path.display())))?;

    let chain = resolve_extends_chain(spec_path, &content).await?;

    let merged = if chain.is_empty() {
        // No extends — parse directly
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content)
            .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?
    } else {
        // Load chain in reverse (grandparent first), shallow-merge each layer on top
        let mut base = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
        for path in chain.iter().rev() {
            let file_content = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", path.display())))?;
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&file_content)
                .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error in {}: {e}", path.display())]))?;
            shallow_merge(&mut base, value);
        }
        // Finally merge the child (spec_path itself) on top
        let child: serde_yaml_ng::Value = serde_yaml_ng::from_str(&content)
            .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?;
        shallow_merge(&mut base, child);
        // Strip the extends key from the merged result
        if let serde_yaml_ng::Value::Mapping(ref mut map) = base {
            map.remove("extends");
        }
        base
    };

    let spec: MirrorSpec = serde_yaml_ng::from_value(merged)
        .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?;

    // Policy check (exit 64 / SpecUsageError) must run before structural validate
    // (exit 65 / SpecInvalid) so the correct exit code is returned for URL-literal
    // webhook secrets.
    if let Some(notify) = &spec.notify {
        policy_check_notify(notify)?;
    }

    let errors = spec.validate(spec_path);
    if !errors.is_empty() {
        return Err(MirrorError::SpecInvalid(errors));
    }

    // Advisory (not fatal): cascade publishing without a build timestamp lets a
    // re-publish orphan the prior digest, which registry GC can reap and break
    // `@sha256:` pins. See `cascade_without_build_stamp` and the build_timestamp
    // reference for GC-safe options (issue #12).
    if spec.cascade_without_build_stamp() {
        log::warn!(
            "[{}] build_timestamp: none with cascade publishing — re-pointing a cascade tag orphans the prior digest, which registry GC can reap and break @sha256: pins; set build_timestamp: date|datetime or configure registry retention (see the mirror.yml build_timestamp reference)",
            spec.name
        );
    }

    Ok(spec)
}

/// Walk the `extends` chain collecting file paths: [parent, grandparent, ...].
/// Detects circular dependencies via `HashSet<PathBuf>`.
///
/// Paths are as joined, not canonicalized — a chain entry may still contain
/// `..`. Callers that compare them against another path (the CI renderer places
/// them under the repository root) canonicalize first.
pub(crate) async fn resolve_extends_chain(
    spec_path: &Path,
    content: &str,
) -> Result<Vec<std::path::PathBuf>, MirrorError> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(content)
        .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?;

    let mapping = match &value {
        serde_yaml_ng::Value::Mapping(m) => m,
        _ => return Ok(vec![]),
    };

    let extends_value = match mapping.get("extends") {
        Some(v) => v,
        None => return Ok(vec![]),
    };

    let spec_dir = spec_path.parent().unwrap_or(Path::new("."));
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(spec_path.canonicalize().unwrap_or_else(|_| spec_path.to_path_buf()));

    // Start with the first extends reference
    let mut current_extends = extends_value.clone();
    let mut current_dir = spec_dir.to_path_buf();

    loop {
        let base_rel = match current_extends.as_str() {
            Some(s) => s.to_string(),
            None => {
                return Err(MirrorError::SpecInvalid(vec![
                    "extends: value must be a string path".to_string(),
                ]));
            }
        };

        let base_path = current_dir.join(&base_rel);
        if !base_path.exists() {
            return Err(MirrorError::SpecInvalid(vec![format!(
                "extends: base file not found: {}",
                base_path.display()
            )]));
        }

        let canonical = base_path.canonicalize().unwrap_or_else(|_| base_path.clone());
        if !seen.insert(canonical) {
            // Build a nice cycle description
            let cycle: Vec<String> = std::iter::once(spec_path.display().to_string())
                .chain(chain.iter().map(|p: &std::path::PathBuf| p.display().to_string()))
                .chain(std::iter::once(base_path.display().to_string()))
                .collect();
            return Err(MirrorError::SpecInvalid(vec![format!(
                "extends: circular dependency: {}",
                cycle.join(" -> ")
            )]));
        }

        chain.push(base_path.clone());

        // Check if the base file also has an extends
        let base_content = tokio::fs::read_to_string(&base_path)
            .await
            .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", base_path.display())))?;
        let base_value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&base_content)
            .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error in {}: {e}", base_path.display())]))?;

        match base_value.as_mapping().and_then(|m| m.get("extends")) {
            Some(next) => {
                current_extends = next.clone();
                current_dir = base_path.parent().unwrap_or(Path::new(".")).to_path_buf();
            }
            None => break,
        }
    }

    Ok(chain)
}

/// Shallow-merge: for each top-level key in `overlay`, replace the corresponding
/// key in `base` entirely. No recursion into nested maps.
fn shallow_merge(base: &mut serde_yaml_ng::Value, overlay: serde_yaml_ng::Value) {
    let base_map = match base {
        serde_yaml_ng::Value::Mapping(m) => m,
        _ => return,
    };
    let overlay_map = match overlay {
        serde_yaml_ng::Value::Mapping(m) => m,
        _ => return,
    };
    for (key, value) in overlay_map {
        base_map.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a spec whose only varying lines are `cascade` / `build_timestamp`,
    /// for exercising [`MirrorSpec::cascade_without_build_stamp`].
    fn spec_with(cascade: &str, build_timestamp: &str) -> MirrorSpec {
        let yaml = format!(
            r#"
name: gctest
target:
  registry: ocx.sh
  repository: gctest
source:
  type: github_release
  owner: o
  repo: r
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "x\\.tar\\.gz"
cascade: {cascade}
build_timestamp: {build_timestamp}
"#
        );
        serde_yaml_ng::from_str(&yaml).unwrap()
    }

    #[test]
    fn cascade_without_build_stamp_flags_only_none_plus_cascade() {
        // The GC-unsafe combination: re-pointable cascade tags, no unique stamp.
        assert!(spec_with("true", "none").cascade_without_build_stamp());

        // A retained per-build tag keeps every digest reachable — safe.
        assert!(!spec_with("true", "date").cascade_without_build_stamp());
        assert!(!spec_with("true", "datetime").cascade_without_build_stamp());

        // No cascade means no rolling tag to re-point — safe even with `none`.
        assert!(!spec_with("false", "none").cascade_without_build_stamp());
    }

    #[test]
    fn parse_github_release_spec() {
        let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*-linux-x86_64\\.tar\\.gz"
  linux/arm64:
    - "cmake-.*-linux-aarch64\\.tar\\.gz"
  darwin/amd64:
    - "cmake-.*-macos-universal\\.tar\\.gz"
  darwin/arm64:
    - "cmake-.*-macos-universal\\.tar\\.gz"
metadata:
  default: metadata/cmake.json
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(spec.name, "cmake");
        assert_eq!(spec.target.registry, "ocx.sh");
        assert_eq!(spec.target.repository, "cmake");
        assert!(matches!(spec.source, Source::GithubRelease { .. }));
        assert_eq!(spec.build_timestamp, BuildTimestampFormat::Datetime);
        assert!(spec.cascade.enabled);
        assert!(!spec.skip_prereleases);
    }

    #[test]
    fn parse_url_index_inline_spec() {
        let yaml = r#"
name: test-tool
target:
  registry: localhost:5000
  repository: test-tool
source:
  type: url_index
  versions:
    "1.0.0":
      assets:
        test-tool-1.0.0-linux-amd64.tar.gz: "https://example.com/test-tool-1.0.0-linux-amd64.tar.gz"
    "1.1.0":
      prerelease: true
      assets:
        test-tool-1.1.0-linux-amd64.tar.gz: "https://example.com/test-tool-1.1.0-linux-amd64.tar.gz"
assets:
  linux/amd64:
    - "test-tool-.*-linux-amd64\\.tar\\.gz"
build_timestamp: date
cascade: false
skip_prereleases: true
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(spec.name, "test-tool");
        assert_eq!(spec.build_timestamp, BuildTimestampFormat::Date);
        assert!(!spec.cascade.enabled);
        assert!(spec.skip_prereleases);

        if let Source::UrlIndex(UrlIndexSource::Inline { versions }) = &spec.source {
            assert_eq!(versions.len(), 2);
            assert!(versions["1.1.0"].prerelease);
        } else {
            panic!("Expected UrlIndex Inline source, got: {:?}", spec.source);
        }
    }

    #[test]
    fn parse_url_index_remote_spec() {
        let yaml = r#"
name: test-tool
target:
  registry: localhost:5000
  repository: test-tool
source:
  type: url_index
  url: "https://example.com/versions.json"
assets:
  linux/amd64:
    - "test-tool-.*-linux-amd64\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        if let Source::UrlIndex(UrlIndexSource::Remote { url }) = &spec.source {
            assert_eq!(url, "https://example.com/versions.json");
        } else {
            panic!("Expected UrlIndex Remote source, got: {:?}", spec.source);
        }
    }

    // ── env sources: pylock / pypi ───────────────────────────────────────────

    #[test]
    fn parse_and_validate_pylock_spec_with_wheels() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheel_scope: acme-wheels
wheels:
  "linux/amd64+libc.glibc": ~
  "linux/amd64+libc.musl": [musllinux, any]
  darwin/arm64: ~
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(spec.source, Source::Pylock { .. }));
        assert_eq!(spec.wheel_scope, "acme-wheels");
        assert!(spec.python.is_some());
        assert_eq!(spec.wheels.as_ref().unwrap().filters.len(), 3);

        let errors = spec.validate(Path::new("test.yaml"));
        assert!(errors.is_empty(), "valid pylock spec should validate: {errors:?}");
    }

    #[test]
    fn pylock_spec_defaults_wheel_scope() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(spec.wheel_scope, "pip-packages");
        assert!(spec.validate(Path::new("test.yaml")).is_empty());
    }

    #[test]
    fn validate_reject_env_spec_without_wheels() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("wheels: required for source.type 'pylock'")),
            "Expected wheels-required error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_env_spec_with_variants() {
        // Breaking (intended): env packages model libc via `+libc.*` wheels
        // keys (os.features platform axis), never via `variants:`.
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
variants:
  - name: musl
    default: true
    assets:
      linux/amd64:
        - "acme-.*-musl\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("variants: not supported for source.type 'pylock'")),
            "Expected variants-on-env error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_wheels_on_archive_source() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
wheels:
  linux/amd64: ~
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("wheels: only supported for source.type 'pylock'/'pypi'")),
            "Expected wheels-on-archive error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_wheels_platforms_cross_coverage() {
        // A wheels key whose base os/arch is not a declared platform leg, and a
        // declared platform leg no wheels key covers — both rejected.
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  "linux/arm64+libc.glibc": ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("base platform 'linux/arm64' is not declared under 'platforms'")),
            "Expected uncovered-wheels-key error, got: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("platforms.linux/amd64: no wheels key covers this platform")),
            "Expected uncovered-platform-leg error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_wheels_dual_libc_keys_cover_one_platform_leg() {
        // The dual-libc shape: two `+libc.*` keys sharing one base cover the
        // same CI matrix leg — one package, one tag, two index entries.
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  "linux/amd64+libc.glibc": ~
  "linux/amd64+libc.musl": ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(errors.is_empty(), "dual-libc keys must validate: {errors:?}");
    }

    #[test]
    fn validate_reject_pylock_missing_python_block() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("python: required")),
            "Expected missing python block error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_pylock_with_top_level_assets() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
assets:
  linux/amd64:
    - "should-not-be-here\\.whl"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("assets") && e.contains("not supported for source.type 'pylock'")),
            "Expected asset-patterns-on-pylock error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_pylock_with_top_level_metadata() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
metadata:
  default: metadata.json
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| *e == metadata_not_supported_error("pylock")),
            "Expected exact metadata-not-supported-for-pylock error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_pypi_with_top_level_metadata() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
metadata:
  default: metadata.json
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| *e == metadata_not_supported_error("pypi")),
            "Expected exact metadata-not-supported-for-pypi error, got: {errors:?}"
        );
    }

    /// `bin_scan` has nowhere to look on an env spec — its content tree is
    /// composed from wheels, never extracted from an archive — so a declared
    /// scan mode is rejected like `metadata:`. `libc_lint` is the deliberate
    /// counter-case: the env prepare pipeline runs it over the composed tree,
    /// so the same spec keeps the check on and must validate clean.
    #[test]
    fn validate_rejects_bin_scan_on_env_spec_but_accepts_inert_libc_lint() {
        let base = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}bin_scan: verify\n")).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| *e == bin_scan_not_supported_error("pypi")),
            "Expected exact bin_scan-not-supported-for-pypi error, got: {errors:?}"
        );

        // `off` is the default every env spec carries without saying so — it
        // must not red, or no env spec would ever validate.
        let spec: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}bin_scan: off\n")).unwrap();
        assert!(spec.validate(Path::new("test.yaml")).is_empty());

        // The libc check stays declarable, in both directions, and on by
        // default — the env leg is where a `+libc.*` key can be contradicted.
        let spec: MirrorSpec = serde_yaml_ng::from_str(base).unwrap();
        assert!(spec.libc_lint, "an unmentioned libc_lint must be on for env specs too");
        for value in ["true", "false"] {
            let spec: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}libc_lint: {value}\n")).unwrap();
            let errors = spec.validate(Path::new("test.yaml"));
            assert!(errors.is_empty(), "libc_lint: {value} must validate on env: {errors:?}");
        }
    }

    #[tokio::test]
    async fn pypi_fixture_spec_loads_and_validates() {
        let spec_path =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pypi.yml"));
        let spec = load_spec(&spec_path)
            .await
            .expect("pypi fixture spec must load and validate");
        assert!(matches!(spec.source, Source::Pypi { .. }));
    }

    #[test]
    fn validate_reject_pypi_missing_python_block() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("python: required")),
            "Expected missing python block error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_pypi_with_top_level_assets() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
assets:
  linux/amd64:
    - "should-not-be-here\\.whl"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("assets") && e.contains("not supported for source.type 'pypi'")),
            "Expected asset-patterns-on-pypi error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_pypi_bad_index_url() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
  index: "ftp://pypi.example.com"
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("source.index") && e.contains("http(s)")),
            "Expected bad index URL error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_pylock_with_python_lock_field() {
        // `python.lock` configures lock *derivation*, which only makes sense
        // for `source.type: pypi` — a `pylock` source already resolves its
        // own committed lock.
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
  lock: {}
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("python.lock") && e.contains("only supported for source.type 'pypi'")),
            "Expected python.lock-on-pylock error, got: {errors:?}"
        );
    }

    #[test]
    fn reject_missing_name() {
        let yaml = r#"
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
"#;

        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn reject_missing_target() {
        let yaml = r#"
name: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
"#;

        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn validate_tag_pattern_without_version_group() {
        let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("version")),
            "Expected version group error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_invalid_regex_in_assets() {
        let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
assets:
  linux/amd64:
    - "[invalid"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("regex")),
            "Expected regex error, got: {errors:?}"
        );
    }

    #[test]
    fn reject_url_index_with_neither_url_nor_versions_nor_generator() {
        let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "Expected parse error for empty url_index");
    }

    #[test]
    fn parse_url_index_generator_spec() {
        let yaml = r#"
name: nodejs
target:
  registry: ocx.sh
  repository: nodejs
source:
  type: url_index
  generator:
    command: ["uv", "run", "generate.py"]
    working_directory: scripts
assets:
  linux/amd64:
    - "node-.*-linux-x64\\.tar\\.xz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        if let Source::UrlIndex(UrlIndexSource::Generator { generator }) = &spec.source {
            assert_eq!(generator.command, vec!["uv", "run", "generate.py"]);
            assert_eq!(generator.working_directory.as_deref(), Some("scripts"));
        } else {
            panic!("Expected UrlIndex Generator source, got: {:?}", spec.source);
        }
    }

    #[test]
    fn parse_url_index_generator_default_working_directory() {
        let yaml = r#"
name: nodejs
target:
  registry: ocx.sh
  repository: nodejs
source:
  type: url_index
  generator:
    command: ["uv", "run", "generate.py"]
assets:
  linux/amd64:
    - "node-.*-linux-x64\\.tar\\.xz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        if let Source::UrlIndex(UrlIndexSource::Generator { generator }) = &spec.source {
            assert!(generator.working_directory.is_none());
            let resolved = generator.resolve_working_directory(Path::new("/mirrors/nodejs"));
            assert_eq!(resolved, Path::new("/mirrors/nodejs"));
        } else {
            panic!("Expected UrlIndex Generator source, got: {:?}", spec.source);
        }
    }

    #[test]
    fn validate_generator_empty_command() {
        let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
  generator:
    command: []
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("non-empty")),
            "Expected empty command error, got: {errors:?}"
        );
    }

    #[test]
    fn default_values() {
        let yaml = r#"
name: minimal
target:
  registry: ocx.sh
  repository: minimal
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(spec.build_timestamp, BuildTimestampFormat::Datetime);
        assert!(spec.cascade.enabled);
        assert!(!spec.skip_prereleases);
        assert!(spec.asset_type.is_none(), "asset_type should default to None");
        assert_eq!(spec.concurrency.max_downloads, 8);
        assert_eq!(spec.concurrency.rate_limit_ms, 0);
        assert_eq!(spec.concurrency.max_retries, 3);
        assert!(!spec.allow_manual_edits, "allow_manual_edits should default to false");
    }

    #[test]
    fn a_spec_that_still_sets_max_pushes_keeps_parsing() {
        // `max_pushes` was removed as a knob nothing read. Every mirror repo in
        // the fleet carries its own `mirror.yml`, so the field outliving the
        // code that named it must stay harmless — which it is only as long as
        // `ConcurrencyConfig` does not deny unknown fields. This pins that.
        let yaml = r#"
name: minimal
target:
  registry: ocx.sh
  repository: minimal
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
concurrency:
  max_pushes: 4
  max_retries: 5
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("a stale `max_pushes` must not break a mirror");
        assert_eq!(spec.concurrency.max_retries, 5);
    }

    #[test]
    fn parse_allow_manual_edits_true() {
        let yaml = r#"
name: minimal
target:
  registry: ocx.sh
  repository: minimal
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
allow_manual_edits: true
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(spec.allow_manual_edits, "allow_manual_edits: true must parse");
    }

    #[test]
    fn default_verify_values() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
verify:
  github_asset_digest: false
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let verify = spec.verify.unwrap();
        assert!(!verify.github_asset_digest);
        assert!(verify.checksums_file.is_none());
    }

    #[test]
    fn parse_asset_type_archive() {
        let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
asset_type:
  type: archive
  strip_components: 1
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        match spec.asset_type.as_ref().unwrap().resolve("linux/amd64") {
            asset_type::AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(1)),
            _ => panic!("expected Archive"),
        }
    }

    #[test]
    fn parse_asset_type_archive_per_platform() {
        let yaml = r#"
name: shellcheck
target:
  registry: ocx.sh
  repository: shellcheck
source:
  type: github_release
  owner: koalaman
  repo: shellcheck
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shellcheck-.*\\.tar\\.xz"
asset_type:
  type: archive
  strip_components:
    default: 1
    platforms:
      windows/amd64: 0
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let at = spec.asset_type.as_ref().unwrap();
        match at.resolve("linux/amd64") {
            asset_type::AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(1)),
            _ => panic!("expected Archive"),
        }
        match at.resolve("windows/amd64") {
            asset_type::AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(0)),
            _ => panic!("expected Archive"),
        }
    }

    #[test]
    fn parse_asset_type_binary() {
        let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        match spec.asset_type.as_ref().unwrap().resolve("linux/amd64") {
            asset_type::AssetType::Binary { name } => assert_eq!(name, "shfmt"),
            _ => panic!("expected Binary"),
        }
    }

    #[test]
    fn reject_url_index_with_both_url_and_versions() {
        let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
  url: "https://example.com/versions.json"
  versions:
    "1.0.0":
      assets:
        test.tar.gz: "https://example.com/test.tar.gz"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "Expected parse error for url_index with both url and versions"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("exactly one"), "Expected 'exactly one' error, got: {err}");
    }

    #[test]
    fn reject_url_index_with_both_url_and_generator() {
        let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
  url: "https://example.com/versions.json"
  generator:
    command: ["echo", "{}"]
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "Expected parse error for url_index with both url and generator"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("exactly one"), "Expected 'exactly one' error, got: {err}");
    }

    #[test]
    fn reject_unknown_source_type() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: unknown_source
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    // -- extends tests --

    #[tokio::test]
    async fn load_spec_without_extends() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("mirror.yml");
        std::fs::write(
            &spec_path,
            r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#,
        )
        .unwrap();

        let spec = load_spec(&spec_path).await.unwrap();
        assert_eq!(spec.name, "test");
        assert!(spec.cascade.enabled);
    }

    #[tokio::test]
    async fn load_spec_extends_happy_path() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("base.yml"),
            r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
cascade: true
build_timestamp: none
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("child.yml"),
            r#"
extends: base.yml
name: child-test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#,
        )
        .unwrap();

        let spec = load_spec(&dir.path().join("child.yml")).await.unwrap();
        assert_eq!(spec.name, "child-test");
        assert_eq!(spec.target.registry, "ocx.sh");
        assert!(spec.cascade.enabled);
        assert_eq!(spec.build_timestamp, BuildTimestampFormat::None);
    }

    #[tokio::test]
    async fn load_spec_extends_shallow_override() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("base.yml"),
            r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "base\\.tar\\.gz"
  darwin/arm64:
    - "base-darwin\\.tar\\.gz"
versions:
  min: "1.0.0"
  new_per_run: 5
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("child.yml"),
            r#"
extends: base.yml
name: child
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
versions:
  min: "8.0.0"
  new_per_run: 10
"#,
        )
        .unwrap();

        let spec = load_spec(&dir.path().join("child.yml")).await.unwrap();
        // versions should be entirely replaced, not deep-merged
        let versions = spec.versions.unwrap();
        assert_eq!(versions.min.as_deref(), Some("8.0.0"));
        assert_eq!(versions.new_per_run, Some(10));
        // assets should still come from base (not overridden)
        assert!(matches!(spec.source, Source::GithubRelease { .. }));
    }

    #[tokio::test]
    async fn load_spec_extends_circular() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("a.yml"),
            r#"
extends: b.yml
name: a
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("b.yml"),
            r#"
extends: a.yml
name: b
"#,
        )
        .unwrap();

        let err = load_spec(&dir.path().join("a.yml")).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("circular dependency"),
            "Expected circular error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn load_spec_extends_file_not_found() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("child.yml"),
            r#"
extends: nonexistent.yml
name: child
"#,
        )
        .unwrap();

        let err = load_spec(&dir.path().join("child.yml")).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("base file not found"),
            "Expected not found error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn load_spec_extends_missing_required_fields() {
        let dir = tempfile::tempdir().unwrap();

        // Base provides target but no source
        std::fs::write(
            dir.path().join("base.yml"),
            r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#,
        )
        .unwrap();

        // Child adds name but no source — merged result is missing required `source`
        std::fs::write(
            dir.path().join("child.yml"),
            r#"
extends: base.yml
name: incomplete
"#,
        )
        .unwrap();

        let err = load_spec(&dir.path().join("child.yml")).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("source") || msg.contains("missing"),
            "Expected missing field error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn load_spec_extends_chain() {
        let dir = tempfile::tempdir().unwrap();

        // grandparent: provides target and assets
        std::fs::write(
            dir.path().join("grandparent.yml"),
            r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
cascade: false
build_timestamp: date
"#,
        )
        .unwrap();

        // parent: extends grandparent, overrides cascade
        std::fs::write(
            dir.path().join("parent.yml"),
            r#"
extends: grandparent.yml
cascade: true
skip_prereleases: true
"#,
        )
        .unwrap();

        // child: extends parent, adds name and source
        std::fs::write(
            dir.path().join("child.yml"),
            r#"
extends: parent.yml
name: chain-test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#,
        )
        .unwrap();

        let spec = load_spec(&dir.path().join("child.yml")).await.unwrap();
        assert_eq!(spec.name, "chain-test");
        assert_eq!(spec.target.registry, "ocx.sh");
        // cascade: grandparent=false, parent=true → true
        assert!(spec.cascade.enabled);
        // build_timestamp: grandparent=date, not overridden → date
        assert_eq!(spec.build_timestamp, BuildTimestampFormat::Date);
        // skip_prereleases: parent=true → true
        assert!(spec.skip_prereleases);
    }

    #[tokio::test]
    async fn load_spec_extends_replaces_cascade_wholesale() {
        // `cascade` is one key, whichever shape it takes: a child spelling the
        // bool must not inherit the base's schedule, or opting a mirror out of
        // repair would leave it on a timer.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("base.yml"),
            r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
cascade:
  schedule: "17 4 * * 1"
"#,
        )
        .unwrap();

        let child_body = r#"
extends: base.yml
name: chain-test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#;
        let child = dir.path().join("child.yml");

        std::fs::write(&child, child_body).unwrap();
        let inherited = load_spec(&child).await.unwrap();
        assert_eq!(inherited.cascade.schedule.as_deref(), Some("17 4 * * 1"));

        std::fs::write(&child, format!("{child_body}cascade: false\n")).unwrap();
        let overridden = load_spec(&child).await.unwrap();
        assert_eq!(
            overridden.cascade,
            CascadeConfig {
                enabled: false,
                schedule: None
            },
        );
    }

    #[tokio::test]
    async fn load_spec_rejects_a_cron_that_could_add_its_own_triggers() {
        // Both cron fields are spliced into a generated workflow's `on:` block
        // inside a single-quoted scalar. A value that closes that scalar adds a
        // trigger of the spec's choosing — and any non-schedule trigger makes
        // the cascade repair run for real, unattended. Reject before render.
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
name: cron-guard
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;
        let spec_path = dir.path().join("mirror.yml");

        for field in ["cascade:\n  schedule", "versions:\n  poll_interval"] {
            std::fs::write(
                &spec_path,
                format!("{body}{field}: \"0 4 * * 1'\\n  push:\\n    branches: [main]\\n#\"\n"),
            )
            .unwrap();
            let err = load_spec(&spec_path).await.expect_err("injected cron must be rejected");
            assert!(matches!(err, MirrorError::SpecInvalid(_)), "{field}: {err}");
        }

        std::fs::write(&spec_path, format!("{body}cascade:\n  schedule: \"17 4 * * 1\"\n")).unwrap();
        load_spec(&spec_path).await.expect("a plain cron must still load");
    }

    // -- variant tests --

    #[test]
    fn parse_spec_with_variants() {
        let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: astral-sh
  repo: python-build-standalone
  tag_pattern: "^(?P<version>\\d+\\.\\d+\\.\\d+)\\+\\d+$"
variants:
  - name: pgo.lto
    default: true
    assets:
      linux/amd64:
        - "cpython-.*-x86_64-.*-pgo\\+lto-.*\\.tar\\.zst"
      darwin/arm64:
        - "cpython-.*-aarch64-apple-darwin-pgo\\+lto-.*\\.tar\\.zst"
  - name: debug
    assets:
      linux/amd64:
        - "cpython-.*-x86_64-.*-debug-.*\\.tar\\.zst"
metadata:
  default: metadata/python.json
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(spec.name, "python");
        assert!(spec.assets.is_none(), "top-level assets should be None");
        let variants = spec.variants.as_ref().unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].name.as_deref(), Some("pgo.lto"));
        assert!(variants[0].default);
        assert_eq!(variants[1].name.as_deref(), Some("debug"));
        assert!(!variants[1].default);
    }

    #[test]
    fn parse_spec_without_variants_backward_compat() {
        let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*-linux-x86_64\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(spec.assets.is_some());
        assert!(spec.variants.is_none());
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(errors.is_empty(), "backward-compat spec should validate: {errors:?}");
    }

    #[test]
    fn validate_reject_both_assets_and_variants() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
variants:
  - name: debug
    default: true
    assets:
      linux/amd64:
        - "test-debug\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("cannot specify both")),
            "Expected mutual exclusivity error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_reject_neither_assets_nor_variants() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("must specify either")),
            "Expected missing assets/variants error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_variant_exactly_one_default() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: debug
    assets:
      linux/amd64:
        - "test-debug\\.tar\\.gz"
  - name: release
    assets:
      linux/amd64:
        - "test-release\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("exactly one variant must be default")),
            "Expected default count error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_variant_two_defaults() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: debug
    default: true
    assets:
      linux/amd64:
        - "test-debug\\.tar\\.gz"
  - name: release
    default: true
    assets:
      linux/amd64:
        - "test-release\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("exactly one variant must be default, found 2")),
            "Expected two-default error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_variant_invalid_name() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: Debug-Build
    default: true
    assets:
      linux/amd64:
        - "test\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("invalid name")),
            "Expected invalid name error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_variant_latest_reserved() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: latest
    default: true
    assets:
      linux/amd64:
        - "test\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("reserved")),
            "Expected reserved name error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_variant_duplicate_names() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: debug
    default: true
    assets:
      linux/amd64:
        - "test\\.tar\\.gz"
  - name: debug
    assets:
      linux/amd64:
        - "test2\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("duplicate")),
            "Expected duplicate name error, got: {errors:?}"
        );
    }

    #[test]
    fn effective_variants_without_variants_key() {
        let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
metadata:
  default: metadata/cmake.json
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let variants = spec.effective_variants();
        assert_eq!(variants.len(), 1);
        assert!(variants[0].name.is_none());
        assert!(variants[0].is_default);
        assert!(variants[0].metadata.is_some());
    }

    #[test]
    fn effective_variants_unnamed_default_with_named_variant() {
        let yaml = r#"
name: cpython
target:
  registry: ocx.sh
  repository: cpython
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - default: true
    assets:
      linux/amd64:
        - "install_only\\.tar\\.gz"
  - name: slim
    assets:
      linux/amd64:
        - "install_only_stripped\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");

        let variants = spec.effective_variants();
        assert_eq!(variants.len(), 2);

        assert!(variants[0].name.is_none());
        assert!(variants[0].is_default);

        assert_eq!(variants[1].name.as_deref(), Some("slim"));
        assert!(!variants[1].is_default);
    }

    #[test]
    fn validate_variant_unnamed_non_default_rejected() {
        let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: release
    default: true
    assets:
      linux/amd64:
        - "release\\.tar\\.gz"
  - assets:
      linux/amd64:
        - "other\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(
            errors.iter().any(|e| e.contains("unnamed variant must be the default")),
            "Expected unnamed-must-be-default error, got: {errors:?}"
        );
    }

    #[test]
    fn effective_variants_with_variants_key() {
        let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: pgo.lto
    default: true
    assets:
      linux/amd64:
        - "pgo-lto-.*\\.tar\\.gz"
  - name: debug
    assets:
      linux/amd64:
        - "debug-.*\\.tar\\.gz"
metadata:
  default: metadata/python.json
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let variants = spec.effective_variants();
        assert_eq!(variants.len(), 2);

        assert_eq!(variants[0].name.as_deref(), Some("pgo.lto"));
        assert!(variants[0].is_default);
        // Inherits top-level metadata
        assert!(variants[0].metadata.is_some());

        assert_eq!(variants[1].name.as_deref(), Some("debug"));
        assert!(!variants[1].is_default);
        // Also inherits top-level metadata
        assert!(variants[1].metadata.is_some());
    }

    #[test]
    fn effective_variants_variant_overrides_metadata() {
        let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: pgo.lto
    default: true
    assets:
      linux/amd64:
        - "pgo-lto-.*\\.tar\\.gz"
    metadata:
      default: metadata/python-pgo.json
  - name: debug
    assets:
      linux/amd64:
        - "debug-.*\\.tar\\.gz"
metadata:
  default: metadata/python.json
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let variants = spec.effective_variants();

        // pgo.lto overrides metadata
        let pgo = &variants[0];
        assert!(pgo.metadata.is_some());

        // debug inherits top-level metadata
        let debug = &variants[1];
        assert!(debug.metadata.is_some());
    }

    /// `bin_scan` follows the same override-with-fallback rule as `metadata`
    /// and `asset_type`: a slim variant ships a different binary set than the
    /// full one, so it may need a different mode than the spec's — and a
    /// variant that says nothing must inherit rather than silently reset to
    /// `off`.
    #[test]
    fn effective_variants_bin_scan_overrides_per_variant_and_falls_back() {
        let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
bin_scan: verify
variants:
  - default: true
    assets:
      linux/amd64:
        - "full-.*\\.tar\\.gz"
  - name: slim
    bin_scan: auto
    assets:
      linux/amd64:
        - "slim-.*\\.tar\\.gz"
  - name: legacy
    bin_scan: off
    assets:
      linux/amd64:
        - "legacy-.*\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
        let variants = spec.effective_variants();

        assert_eq!(
            variants[0].bin_scan,
            BinScanMode::Verify,
            "a variant that states no mode inherits the spec's",
        );
        assert_eq!(variants[1].bin_scan, BinScanMode::Auto, "a variant may override it");
        assert_eq!(
            variants[2].bin_scan,
            BinScanMode::Off,
            "including overriding it back off — `off` must not read as 'unset'",
        );
    }

    /// A `bin_scan` on a *variant* must be gated even when the spec-level mode
    /// is `off` — checking only `self.bin_scan` lets exactly the interesting
    /// case through, and the slim variant then publishes `binaries: []`.
    ///
    /// The default variant in the same spec keeps a bare `${installPath}` and
    /// must stay silent, or the gate is just rejecting the file globally.
    #[test]
    fn a_scanning_variant_is_gated_even_when_the_spec_level_mode_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        for (file, value) in [("metadata.json", "${installPath}"), ("slim.json", "${installPath}")] {
            std::fs::write(
                dir.path().join(file),
                format!(
                    r#"{{"type":"bundle","version":1,"env":[
                       {{"key":"PATH","type":"path","required":true,"value":"{value}","visibility":"public"}}]}}"#
                ),
            )
            .unwrap();
        }

        let yaml = r#"
name: tool
target:
  registry: ocx.sh
  repository: tool
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
metadata:
  default: metadata.json
variants:
  - default: true
    assets:
      linux/amd64: ["full-.*\\.tar\\.gz"]
  - name: slim
    bin_scan: auto
    metadata:
      default: slim.json
    assets:
      linux/amd64: ["slim-.*\\.tar\\.gz"]
"#;
        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
        assert_eq!(spec.bin_scan, BinScanMode::Off, "the spec level must stay off");

        let errors = spec.validate(&dir.path().join("mirror.yml"));
        assert_eq!(
            errors.len(),
            1,
            "exactly the scanning variant must be reported: {errors:?}"
        );
        assert!(
            errors[0].starts_with("variants.slim.bin_scan:") && errors[0].contains("slim.json"),
            "the error must name the variant and its file: {}",
            errors[0],
        );
    }

    /// The control: the same unscannable metadata with the scan off is a
    /// perfectly good spec and must keep loading. Without this the gate could
    /// be rejecting on the metadata shape alone and nobody would notice.
    #[test]
    fn a_bare_install_path_var_loads_fine_when_the_scan_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("metadata.json"),
            r#"{"type":"bundle","version":1,"env":[
               {"key":"PATH","type":"path","required":true,"value":"${installPath}","visibility":"public"}]}"#,
        )
        .unwrap();

        let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+)$"
metadata:
  default: metadata.json
asset_type:
  type: binary
  name: shfmt
assets:
  linux/amd64:
    - "shfmt_.*_linux_amd64$"
"#;
        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
        let errors = spec.validate(&dir.path().join("mirror.yml"));
        assert!(errors.is_empty(), "bin_scan: off must not gate anything: {errors:?}");
    }

    /// Omitting the key everywhere must leave every variant unscanned: turning
    /// a scan on by default would start publishing a `binaries` claim no
    /// publisher made, across the whole fleet, on the next cron run.
    #[test]
    fn bin_scan_defaults_to_off_for_a_spec_that_never_mentions_it() {
        let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "shfmt_.*_linux_amd64$"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
        assert_eq!(spec.bin_scan, BinScanMode::Off);
        assert_eq!(spec.effective_variants()[0].bin_scan, BinScanMode::Off);
    }

    /// The opposite default to `bin_scan`'s, and the assertion that has to
    /// break if anyone flips it: a spec that never mentions `libc_lint` is
    /// checked. Every ported spec's declared `os.features` already match their
    /// binaries, so on-by-default reds nothing that ships — and a check the
    /// whole fleet leaves off is not a check.
    #[test]
    fn libc_lint_defaults_to_on_for_a_spec_that_never_mentions_it() {
        let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "shfmt_.*_linux_amd64$"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
        assert!(spec.libc_lint, "an unmentioned libc_lint must be on");
        assert!(spec.effective_variants()[0].libc_lint, "and must reach the variant");
    }

    /// `libc_lint` follows the same override-with-fallback rule as `bin_scan`:
    /// one variant's upstream build can be the only one the check misreads, and
    /// bypassing the whole spec to get that variant through would silently stop
    /// checking the others. A variant that says nothing inherits rather than
    /// resetting to the type default.
    #[test]
    fn effective_variants_libc_lint_overrides_per_variant_and_falls_back() {
        let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
libc_lint: false
variants:
  - default: true
    assets:
      linux/amd64:
        - "full-.*\\.tar\\.gz"
  - name: slim
    libc_lint: true
    assets:
      linux/amd64:
        - "slim-.*\\.tar\\.gz"
"#;

        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
        let variants = spec.effective_variants();

        assert!(
            !variants[0].libc_lint,
            "a variant saying nothing inherits the spec-level value"
        );
        assert!(variants[1].libc_lint, "a variant may override it");
        assert!(
            !spec.libc_lint,
            "the spec level must stay off while one variant turns it on"
        );

        // Both fallback directions, because `true` is also `bool`'s type
        // default: the spec above kills a hardcoded `unwrap_or(true)`, and only
        // a spec that omits the key kills `unwrap_or_default()`. Either
        // mutation survives the other case.
        let inheriting_on = yaml
            .replace("libc_lint: false\n", "")
            .replace("    libc_lint: true\n", "");
        let spec: MirrorSpec = serde_yaml_ng::from_str(&inheriting_on).expect("spec parses");
        assert!(
            spec.effective_variants().iter().all(|v| v.libc_lint),
            "with the key omitted every variant inherits the on default"
        );
    }

    /// A misspelled variant key must be rejected, naming the key. The escape
    /// hatch is the reason this matters: `libc-lint: false` is the spelling the
    /// docs put in front of operators (`ocx package create --no-libc-lint`), and
    /// silently dropping it leaves the check on, the build still refusing, and
    /// no way to tell that the bypass never applied. The same misspelling at the
    /// top level has always been a hard error.
    #[test]
    fn unknown_variant_key_is_rejected_and_named() {
        let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - default: true
    libc-lint: false
    assets:
      linux/amd64:
        - "full-.*\\.tar\\.gz"
"#;

        let err = serde_yaml_ng::from_str::<MirrorSpec>(yaml).expect_err("a misspelled variant key must red");
        let msg = err.to_string();
        assert!(
            msg.contains("libc-lint"),
            "the error must name the offending key: {msg}"
        );

        // The correct spelling still parses — otherwise this test would pass
        // just as well against a parser that rejects every variant.
        let spec: MirrorSpec =
            serde_yaml_ng::from_str(&yaml.replace("libc-lint:", "libc_lint:")).expect("the declared key parses");
        assert!(!spec.effective_variants()[0].libc_lint);
    }

    // ── §3.1 S1: Pipeline schema round-trip and validation tests ────────────

    /// Helper: base YAML suitable for all §3.1 round-trip tests. Adds the
    /// minimum required fields so pipeline-specific blocks can be appended.
    const MINIMAL_BASE_YAML: &str = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
  linux/arm64:
    - "shfmt_v.*_linux_arm64$"
  darwin/arm64:
    - "shfmt_v.*_darwin_arm64$"
asset_type:
  type: binary
  name: shfmt
"#;

    #[test]
    fn round_trip_full_pipeline_spec() {
        // §3.1: Round-trip: valid mirror.yml with full tests:, platforms:,
        // ocx_mirror:, notify: blocks parses correctly.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
  - name: smoke
    command: bash ./tests/smoke.sh

platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
      - image: alpine:3.20
        shell: sh
  darwin/arm64:
    runner: macos-latest
    shell: bash
  windows/amd64:
    runner: windows-latest
    shell: pwsh
    tests:
      - name: version
        command: shfmt.exe --version

ocx_mirror:
  rev: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();

        // tests block
        let tests = spec.tests.as_ref().unwrap();
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].name, "version");
        assert_eq!(tests[0].command.as_deref(), Some("shfmt --version"));
        assert_eq!(tests[1].name, "smoke");

        // platforms block
        let platforms = spec.platforms.as_ref().unwrap();
        assert!(platforms.contains_key("linux/amd64"));
        assert!(platforms.contains_key("darwin/arm64"));
        assert!(platforms.contains_key("windows/amd64"));

        let linux = &platforms["linux/amd64"];
        assert_eq!(linux.runner, "ubuntu-latest");
        let containers = linux.containers.as_ref().unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].image, "ubuntu:24.04");
        assert_eq!(containers[0].shell.as_deref(), Some("bash"));
        assert_eq!(containers[1].image, "alpine:3.20");

        // per-platform test override
        let windows = &platforms["windows/amd64"];
        let win_tests = windows.tests.as_ref().unwrap();
        assert_eq!(win_tests.len(), 1);
        assert_eq!(win_tests[0].name, "version");
        assert_eq!(win_tests[0].command.as_deref(), Some("shfmt.exe --version"));

        // ocx_mirror block
        let ocx_mirror = spec.ocx_mirror.as_ref().unwrap();
        assert_eq!(
            ocx_mirror.rev.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );

        // notify block
        let notify = spec.notify.as_ref().unwrap();
        let discord = notify.discord.as_ref().unwrap();
        assert_eq!(discord.webhook_secret, "DISCORD_WEBHOOK_URL");
    }

    #[test]
    fn validate_empty_tests_array() {
        // §3.1: Rejection — empty tests: array
        let yaml = format!(
            r#"{base}
tests: []
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("tests") && (e.contains("empty") || e.contains("least"))),
            "Expected error about empty tests:, got: {errors:?}"
        );
    }

    #[test]
    fn validate_duplicate_test_names() {
        // §3.1: Rejection — duplicate tests[].name
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
  - name: version
    command: shfmt --version --again
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("duplicate") || e.contains("unique")),
            "Expected duplicate test name error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_invalid_test_name_starts_with_digit() {
        // §3.1: Rejection — invalid tests[].name (starts with digit)
        let yaml = format!(
            r#"{base}
tests:
  - name: 1version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("name") || e.contains("invalid")),
            "Expected invalid test name error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_invalid_platform_key_no_arch() {
        // §3.1: Rejection — invalid platform key (linux without arch)
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux:
    runner: ubuntu-latest
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("platform") || e.contains("linux")),
            "Expected invalid platform key error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_platform_missing_runner() {
        // §3.1: Rejection — missing runner on declared platform
        // PlatformConfig.runner is required (non-optional) so this fails at
        // parse time with serde error.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
            base = MINIMAL_BASE_YAML
        );

        // Missing required `runner` field → serde parse error
        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(&yaml);
        assert!(result.is_err(), "Expected parse error for missing runner, but got Ok");
    }

    #[test]
    fn validate_empty_containers_array() {
        // §3.1: Rejection — empty containers: array (must be absent OR ≥1)
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers: []
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("container") && (e.contains("empty") || e.contains("least"))),
            "Expected error about empty containers:, got: {errors:?}"
        );
    }

    #[test]
    fn validate_ambiguous_shell_on_nonstandard_image() {
        // §3.1: Rejection — ambiguous shell on non-standard image (no default)
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: mycorp/custom-runner:1.0
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("shell") || e.contains("ambiguous")),
            "Expected ambiguous shell error for non-standard image, got: {errors:?}"
        );
    }

    #[test]
    fn validate_platform_rejects_variant_prefixed_min_version() {
        // Applicability keys off the release core; a variant-prefixed bound would
        // compare asymmetrically against the stripped version and silently misfilter.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    min_version: "debug-0.11.7"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("min_version") && e.contains("plain version")),
            "variant-prefixed min_version must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn validate_platform_rejects_build_stamped_max_version() {
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    max_version: "1.0.0_20260101"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("max_version") && e.contains("plain version")),
            "build-stamped max_version must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn validate_platform_rejects_inverted_window() {
        // min ≥ max silently drops the platform from every version — must error.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    min_version: "5.0.0"
    max_version: "2.0.0"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("min_version") && e.contains("must be below")),
            "inverted min/max window must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_containers_on_a_non_linux_platform() {
        // Container legs are `docker run` on a Linux runner. A macOS or Windows
        // runner has no Linux container engine, so the pairing can only fail at
        // run time — after a full matrix spin-up. Reject it at generate time.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  windows/amd64:
    runner: windows-latest
    containers:
      - image: ubuntu:24.04
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("containers are linux-only")),
            "containers on a windows platform must be rejected, got: {errors:?}"
        );
    }

    /// A minimal spec whose single container carries the given extra lines,
    /// indented to sit under `- image: alpine:3.20`.
    fn spec_with_container_lines(lines: &str) -> MirrorSpec {
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
{lines}"#,
            base = MINIMAL_BASE_YAML
        );
        serde_yaml_ng::from_str(&yaml).unwrap()
    }

    #[test]
    fn validate_accepts_setup_commands_on_a_container() {
        let spec = spec_with_container_lines(
            r#"        setup:
          - apk add --no-cache libstdc++
          - apk add --no-cache libgcc
"#,
        );
        let errors = spec.validate(Path::new("test.yml"));
        assert!(errors.is_empty(), "setup commands must validate, got: {errors:?}");
    }

    #[test]
    fn validate_rejects_an_empty_setup_list() {
        // `setup: []` reads as "provision nothing" but declares intent to
        // provision — the maintainer meant to fill it in.
        let spec = spec_with_container_lines("        setup: []\n");
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("empty setup list")),
            "an empty setup list must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_a_blank_setup_command() {
        let spec = spec_with_container_lines(
            r#"        setup:
          - "  "
"#,
        );
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("setup[0] must not be blank")),
            "a blank setup command must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_a_multi_line_setup_command() {
        // One entry is one `RUN`. A block scalar here — the shape `script_inline`
        // trains maintainers to reach for — would emit a broken Dockerfile.
        let spec = spec_with_container_lines(
            r#"        setup:
          - |
            apk add --no-cache libstdc++
            apk add --no-cache libgcc
"#,
        );
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("setup[0] must be a single command")),
            "a multi-line setup command must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_a_trailing_backslash_setup_command() {
        // A line continuation absorbs the next `RUN` as its own arguments: the
        // build exits 0 with that layer never applied, and the leg goes green
        // on an image the setup did not provision.
        // Both spellings: whitespace after the backslash does not stop docker
        // continuing the line, so it must not stop the check either.
        for trailer in ["", " "] {
            let spec = spec_with_container_lines(&format!(
                r#"        setup:
          - "apk add --no-cache libstdc++ \\{trailer}"
          - apk add --no-cache libgcc
"#
            ));
            let errors = spec.validate(Path::new("test.yml"));
            assert!(
                errors
                    .iter()
                    .any(|e| e.contains("setup[0] must not end with a backslash")),
                "a trailing-backslash setup command must be rejected (trailer {trailer:?}), got: {errors:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_setup_on_a_platform_without_containers() {
        // `setup:` belongs to a container, not a platform. One level of
        // under-indentation is the whole mistake, and `deny_unknown_fields` is
        // what turns it into a parse error instead of a dropped line — so this
        // never reaches `validate()`.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    setup:
      - apk add --no-cache libstdc++
"#,
            base = MINIMAL_BASE_YAML
        );
        let error = serde_yaml_ng::from_str::<MirrorSpec>(&yaml)
            .expect_err("a platform-level setup must fail to parse")
            .to_string();
        assert!(
            error.contains("unknown field") && error.contains("setup"),
            "the error must name the rejected key, got: {error}"
        );
    }

    #[test]
    fn validate_accepts_a_libc_bearing_platform_key() {
        // Declaring a libc is the only way to make the claim testable, so the
        // key grammar has to admit it — a `^[a-z0-9_-]+/[a-z0-9_-]+$` regex does
        // not, and that alone kept every libc mirror out of the test matrix.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  "linux/amd64+libc.musl":
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.is_empty(),
            "a musl claim tested in an alpine image must validate, got: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_a_container_whose_libc_contradicts_the_platform_key() {
        // The silent direction: a musl-static artifact runs fine under glibc, so
        // testing a `+libc.musl` claim inside ubuntu goes GREEN having verified
        // nothing. The mismatch must be named, not rendered.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  "linux/amd64+libc.musl":
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("ubuntu:24.04") && e.contains("libc.glibc") && e.contains("libc.musl")),
            "the error must name the image and both libcs, got: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_the_loud_libc_mismatch_too() {
        // The other direction fails at run time rather than passing falsely, but
        // it is the same authoring mistake — reject it symmetrically instead of
        // spending a matrix run to learn it.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  "linux/amd64+libc.glibc":
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("alpine:3.20") && e.contains("libc.musl")),
            "an alpine image under a glibc claim must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn platform_slug_separates_libc_variants_and_leaves_plain_keys_alone() {
        // The join key between `pipeline prepare` (work-dir basename), the CI
        // renderer (bundle + JUnit filenames) and `pipeline push` (lookup).
        assert_eq!(platform_key_slug("linux/amd64+libc.musl"), "linux_amd64_libc.musl");
        assert_eq!(platform_key_slug("linux/amd64+libc.glibc"), "linux_amd64_libc.glibc");
        // Plain keys keep exactly the slug they had — this is what the pinned
        // mirror corpus renders with.
        for key in ["linux/amd64", "linux/arm64", "darwin/arm64", "windows/amd64"] {
            assert_eq!(platform_key_slug(key), key.replace('/', "_"));
        }

        // Docker never sees the suffix; everything else keeps it.
        assert_eq!(platform_without_features("linux/amd64+libc.musl"), "linux/amd64");
        assert_eq!(platform_without_features("linux/arm64"), "linux/arm64");
    }

    #[test]
    fn image_inference_keys_off_the_repository_basename() {
        // A registry-qualified reference must classify like its bare form —
        // otherwise a mirror that spells out `docker.io/library/alpine` gets a
        // gnu ocx that cannot start under musl, and a `bash` that is not there.
        assert_eq!(infer_libc_from_image("alpine:3.20"), "musl");
        assert_eq!(infer_libc_from_image("docker.io/library/alpine:3.20"), "musl");
        assert_eq!(infer_libc_from_image("ubuntu:24.04"), "gnu");
        assert_eq!(infer_libc_from_image("fedora:40"), "gnu");
        assert_eq!(infer_shell_from_image("docker.io/library/alpine:3.20"), Some("sh"));
        assert_eq!(infer_shell_from_image("ghcr.io/acme/fedora:40"), Some("bash"));

        // The join key with `pipeline push`: dots slugify too.
        assert_eq!(image_to_container_id("ubuntu:24.04"), "ubuntu_24_04");
        assert_eq!(image_to_container_id("alpine:3.20"), "alpine_3_20");
        assert_eq!(image_to_container_id("ghcr.io/acme/img:1.0"), "ghcr_io_acme_img_1_0");
    }

    #[test]
    fn validate_exclude_rejects_inverted_range_and_variant_version() {
        // exclude[0]: inverted range matches nothing (silent no-op).
        // exclude[1]: variant-prefixed single version compares asymmetrically.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    exclude:
      - min_version: "9.4.0"
        max_version: "5.0.0"
      - version: "debug-1.0.0"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("exclude[0]") && e.contains("must be below")),
            "inverted exclude range must be rejected, got: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("exclude[1]") && e.contains("plain version")),
            "variant-prefixed exclude version must be rejected, got: {errors:?}"
        );
    }

    #[test]
    fn containers_need_no_ocx_mirror_block() {
        // Declaring `containers:` once demanded an `ocx_mirror.release_tag`
        // that nothing rendered — a required field with no consumer. The ocx
        // the legs download is the renderer's own `OCX_CONTAINER_CLI_TAG`, so
        // the spec has nothing to say about it. A container spec with no
        // `ocx_mirror:` block at all must validate clean.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert!(spec.ocx_mirror.is_none(), "fixture must carry no ocx_mirror block");
        let errors = spec.validate(Path::new("test.yml"));
        assert!(errors.is_empty(), "expected a clean spec, got: {errors:?}");
    }

    #[test]
    fn validate_rev_not_40_hex() {
        // §3.1: Rejection — rev not 40-hex
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
ocx_mirror:
  rev: "short"
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("rev") || e.contains("hex") || e.contains("40")),
            "Expected invalid rev format error, got: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_ocx_install_block() {
        // §3.1: Rejection — ocx_install: block present at all → SpecUsageError
        // Catches early adopters who copied an earlier draft spec.
        // Since ocx_install is not in the schema, serde rejects unknown fields
        // OR it silently ignores them (depends on #[serde(deny_unknown_fields)]).
        // We test via validate() returning an error for this field.
        //
        // Implementation note: the validator should check for `ocx_install` key
        // via a raw YAML pass or a dedicated sentinel field, and emit:
        //   "ocx binary is installed via direct release download; remove `ocx_install:` block"
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
ocx_install: {{}}
"#,
            base = MINIMAL_BASE_YAML
        );

        // If serde rejects unknown fields, this is a parse error.
        // If serde ignores unknown fields, it's a validate() error.
        // Either satisfies the rejection requirement.
        let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(&yaml);
        match result {
            Err(_) => {
                // serde rejected the unknown field — test passes
            }
            Ok(spec) => {
                let errors = spec.validate(Path::new("test.yml"));
                assert!(
                    errors
                        .iter()
                        .any(|e| e.contains("ocx_install") || e.contains("release download")),
                    "Expected rejection of ocx_install: block, got: {errors:?}"
                );
            }
        }
    }

    // ── Per-platform version applicability ─────────────────────────────────

    /// A spec exercising every applicability lever: an undeclared platform
    /// (linux/amd64), a late-introduced platform with a broken single exclude
    /// (windows/arm64), and a dropped platform with an open-ended skip range
    /// (darwin/amd64).
    fn spec_with_platform_windows() -> MirrorSpec {
        let yaml = format!(
            r#"{base}
platforms:
  linux/amd64:
    runner: ubuntu-latest
  windows/arm64:
    runner: windows-11-arm
    min_version: "0.11.7"
    exclude:
      - version: "0.16.0"
        reason: "aarch64-windows build-exe segfault"
        severity: broken
  darwin/amd64:
    runner: macos-14
    max_version: "11.1.0"
    exclude:
      - max_version: "9.4.0"
        severity: skip
"#,
            base = MINIMAL_BASE_YAML
        );
        serde_yaml_ng::from_str(&yaml).expect("applicability spec must parse")
    }

    #[test]
    fn validate_accepts_platform_applicability_window() {
        let spec = spec_with_platform_windows();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(errors.is_empty(), "valid applicability spec must not error: {errors:?}");
    }

    #[test]
    fn platform_applies_respects_min_inclusive() {
        let spec = spec_with_platform_windows();
        assert!(
            !spec.platform_applies("0.11.6", "windows/arm64"),
            "below min is dropped"
        );
        assert!(spec.platform_applies("0.11.7", "windows/arm64"), "min is inclusive");
        assert!(spec.platform_applies("0.12.0", "windows/arm64"));
    }

    #[test]
    fn platform_applies_respects_max_exclusive() {
        let spec = spec_with_platform_windows();
        assert!(spec.platform_applies("11.0.0", "darwin/amd64"));
        assert!(!spec.platform_applies("11.1.0", "darwin/amd64"), "max is exclusive");
        assert!(!spec.platform_applies("12.0.0", "darwin/amd64"));
    }

    #[test]
    fn platform_applies_drops_single_and_range_excludes() {
        let spec = spec_with_platform_windows();
        assert!(
            !spec.platform_applies("0.16.0", "windows/arm64"),
            "single exclude dropped"
        );
        assert!(spec.platform_applies("0.17.0", "windows/arm64"), "outside exclude kept");
        // darwin/amd64 open-ended `max_version: 9.4.0` skip range.
        assert!(!spec.platform_applies("9.3.0", "darwin/amd64"), "range exclude dropped");
        assert!(spec.platform_applies("9.4.0", "darwin/amd64"), "range max is exclusive");
    }

    #[test]
    fn platform_applies_true_for_undeclared_or_unconstrained_platform() {
        let spec = spec_with_platform_windows();
        // Declared but no bounds/excludes.
        assert!(spec.platform_applies("0.1.0", "linux/amd64"));
        // Not declared in `platforms:` at all.
        assert!(spec.platform_applies("0.1.0", "linux/arm64"));
    }

    #[test]
    fn platform_applies_strips_build_metadata() {
        let spec = spec_with_platform_windows();
        // A build-stamped run version compares on its release core.
        assert!(!spec.platform_applies("0.16.0_20260604120000", "windows/arm64"));
        assert!(spec.platform_applies("0.17.0_20260604120000", "windows/arm64"));
    }

    #[test]
    fn exclude_hit_reports_matching_entry_with_severity_and_reason() {
        let spec = spec_with_platform_windows();
        let hit = spec.exclude_hit("0.16.0", "windows/arm64").expect("0.16.0 is excluded");
        assert_eq!(hit.severity, Severity::Broken);
        assert_eq!(hit.reason.as_deref(), Some("aarch64-windows build-exe segfault"));

        // Build-stamped version still resolves to the entry.
        assert!(spec.exclude_hit("0.16.0_20260604", "windows/arm64").is_some());

        let skip = spec.exclude_hit("9.3.0", "darwin/amd64").expect("9.3.0 is excluded");
        assert_eq!(skip.severity, Severity::Skip);

        assert!(
            spec.exclude_hit("0.17.0", "windows/arm64").is_none(),
            "non-excluded → None"
        );
        assert!(
            spec.exclude_hit("0.16.0", "linux/amd64").is_none(),
            "platform has no excludes"
        );
    }

    #[test]
    fn platform_applies_ignores_variant_prefix() {
        let spec = spec_with_platform_windows();
        // Variant mirrors (e.g. cpython `debug`/`pgo.lto`) key off variant-prefixed
        // version strings. Applicability compares on the release core regardless.
        assert!(
            !spec.platform_applies("debug-0.16.0", "windows/arm64"),
            "single exclude dropped under variant"
        );
        assert!(
            !spec.platform_applies("debug-0.11.6", "windows/arm64"),
            "below min dropped under variant"
        );
        assert!(
            spec.platform_applies("debug-0.11.7", "windows/arm64"),
            "min inclusive under variant"
        );
        // darwin/amd64 open-ended range exclude `max_version: 9.4.0`.
        assert!(
            !spec.platform_applies("debug-9.3.0", "darwin/amd64"),
            "range exclude dropped under variant"
        );
        // Variant + build stamp together.
        assert!(!spec.platform_applies("debug-0.16.0_20260604120000", "windows/arm64"));
    }

    #[test]
    fn exclude_hit_matches_variant_prefixed_version() {
        let spec = spec_with_platform_windows();
        // Single-version exclude branch.
        let hit = spec
            .exclude_hit("debug-0.16.0", "windows/arm64")
            .expect("variant version resolves single exclude");
        assert_eq!(hit.severity, Severity::Broken);
        assert!(spec.exclude_hit("debug-0.16.0_20260604", "windows/arm64").is_some());
        // Range exclude branch (darwin/amd64 open-ended max 9.4.0, skip).
        let skip = spec
            .exclude_hit("debug-9.3.0", "darwin/amd64")
            .expect("variant version in range exclude");
        assert_eq!(skip.severity, Severity::Skip);
    }

    #[test]
    fn validate_rejects_unparseable_platform_bounds() {
        let yaml = format!(
            r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    min_version: "not-a-version"
    max_version: "also bad"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("min_version") && e.contains("not a valid version")),
            "bad min_version must error: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("max_version") && e.contains("not a valid version")),
            "bad max_version must error: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_exclude_with_version_and_range() {
        let yaml = format!(
            r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - version: "1.0.0"
        max_version: "2.0.0"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("exclude[0]") && e.contains("cannot set both")),
            "version + range must error: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_empty_exclude_entry() {
        let yaml = format!(
            r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - reason: "no bounds at all"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("exclude[0]") && e.contains("must set")),
            "empty exclude entry must error: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_invalid_exclude_version() {
        let yaml = format!(
            r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - version: "garbage"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("exclude[0]") && e.contains("not a valid version")),
            "unparseable exclude version must error: {errors:?}"
        );
    }

    // ── notify.discord.user_id ─────────────────────────────────────────────

    #[test]
    fn validate_accepts_valid_discord_user_id() {
        let yaml = format!(
            r#"{base}
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "123456789012345678"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            !errors.iter().any(|e| e.contains("user_id")),
            "valid snowflake must not error: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_non_numeric_discord_user_id() {
        let yaml = format!(
            r#"{base}
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "12345"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("user_id") && e.contains("valid Discord user ID")),
            "short snowflake must error: {errors:?}"
        );
    }

    #[test]
    fn policy_check_rejects_user_id_url_and_at_mention() {
        for (user_id, label) in [("https://discord.com/users/1", "URL"), ("@maintainer", "@mention")] {
            let yaml = format!(
                r#"{base}
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "{user_id}"
"#,
                base = MINIMAL_BASE_YAML
            );
            let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
            let result = policy_check_notify(spec.notify.as_ref().unwrap());
            assert!(
                matches!(result, Err(MirrorError::SpecUsageError(_))),
                "user_id {label} must be a usage error: {result:?}"
            );
        }
    }

    #[test]
    fn validate_r3_discord_com_in_webhook_secret() {
        // §3.1 R3 mitigation: webhook_secret containing "discord.com" → rejected
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: "https://discord.com/api/webhooks/1234/token"
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("webhook_secret") || e.contains("discord") || e.contains("URL")),
            "Expected R3 rejection for discord.com URL in webhook_secret, got: {errors:?}"
        );
    }

    #[test]
    fn validate_r3_discordapp_com_in_webhook_secret() {
        // §3.1 R3 mitigation: webhook_secret containing "discordapp.com" → rejected
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: "https://discordapp.com/api/webhooks/1234/token"
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("webhook_secret") || e.contains("discordapp") || e.contains("URL")),
            "Expected R3 rejection for discordapp.com URL in webhook_secret, got: {errors:?}"
        );
    }

    #[test]
    fn validate_r3_http_url_in_webhook_secret() {
        // §3.1 R3 mitigation: webhook_secret matching ^https?:// → rejected
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: "https://example.com/webhook/abc123"
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("webhook_secret") || e.contains("https") || e.contains("URL")),
            "Expected R3 rejection for http:// URL in webhook_secret, got: {errors:?}"
        );
    }

    #[test]
    fn validate_r3_valid_secret_name_accepted() {
        // §3.1 R3 positive: valid GHA secret name accepted without error
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        // No webhook_secret errors expected
        let webhook_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.contains("webhook_secret") || e.contains("discord"))
            .collect();
        assert!(
            webhook_errors.is_empty(),
            "Unexpected webhook_secret errors for valid GHA secret name: {webhook_errors:?}"
        );
    }

    #[test]
    fn annotations_block_parses_and_defaults_to_empty() {
        let bare: MirrorSpec = serde_yaml_ng::from_str(MINIMAL_BASE_YAML).unwrap();
        assert!(bare.annotations.is_empty());

        let yaml = format!(
            r#"{base}
annotations:
  org.opencontainers.image.licenses: Apache-2.0
  org.opencontainers.image.source: https://github.com/upstream/project
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(spec.annotations["org.opencontainers.image.licenses"], "Apache-2.0");
        assert_eq!(
            spec.annotations["org.opencontainers.image.source"],
            "https://github.com/upstream/project"
        );
        assert!(spec.validate(Path::new("test.yml")).is_empty());
    }

    #[test]
    fn validate_rejects_annotation_key_containing_equals() {
        // `KEY=VALUE` is the wire form; a `=` in the key would re-split wrong
        // and publish a key the spec never asked for.
        let yaml = format!(
            r#"{base}
annotations:
  "bad=key": value
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("bad=key")),
            "expected rejection of '=' in annotation key, got: {errors:?}"
        );
    }

    #[test]
    fn validate_per_platform_tests_override_replaces_top_level() {
        // §3.1: Per-platform tests: override replaces top-level entirely (no merge)
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
  - name: smoke
    command: bash ./tests/smoke.sh
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
  windows/amd64:
    runner: windows-latest
    shell: pwsh
    tests:
      - name: version
        command: shfmt.exe --version
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let platforms = spec.platforms.as_ref().unwrap();

        // Top-level tests: 2 entries
        let top_tests = spec.tests.as_ref().unwrap();
        assert_eq!(top_tests.len(), 2);

        // windows/amd64 override: 1 entry only (replacement, not merge)
        let windows = &platforms["windows/amd64"];
        let win_tests = windows.tests.as_ref().unwrap();
        assert_eq!(
            win_tests.len(),
            1,
            "Per-platform override must replace, not merge top-level tests"
        );
        assert_eq!(win_tests[0].name, "version");

        // linux/amd64 has no override — platforms[].tests is None
        let linux = &platforms["linux/amd64"];
        assert!(
            linux.tests.is_none(),
            "linux/amd64 must inherit top-level tests (no override)"
        );
    }

    #[test]
    fn validate_default_shell_alpine_infers_sh() {
        // §3.1: Default-from-image shell inference: alpine:3.20 → sh
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        // alpine:3.20 has a known default (sh) — no ambiguous shell error expected
        let shell_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.contains("shell") || e.contains("ambiguous"))
            .collect();
        assert!(
            shell_errors.is_empty(),
            "alpine:3.20 should have inferred shell 'sh'; got errors: {shell_errors:?}"
        );
    }

    #[test]
    fn validate_default_shell_ubuntu_infers_bash() {
        // §3.1: Default-from-image shell inference: ubuntu:24.04 → bash
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
"#,
            base = MINIMAL_BASE_YAML
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        let shell_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.contains("shell") || e.contains("ambiguous"))
            .collect();
        assert!(
            shell_errors.is_empty(),
            "ubuntu:24.04 should have inferred shell 'bash'; got errors: {shell_errors:?}"
        );
    }

    // ── §TestEntry union: parse + validation ─────────────────────────────────

    #[test]
    fn parse_test_entry_command_kind() {
        // Happy path: `command:` field → TestKind::Command
        let yaml = r#"name: version
command: shfmt --version
"#;
        let entry: TestEntry = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(entry.name, "version");
        assert_eq!(entry.command.as_deref(), Some("shfmt --version"));
        assert!(entry.script.is_none());
        assert!(entry.script_inline.is_none());
        let kind = entry.kind().unwrap();
        assert_eq!(kind, TestKind::Command("shfmt --version"));
    }

    #[test]
    fn parse_test_entry_script_kind() {
        // Happy path: `script:` field → TestKind::Script
        let yaml = r#"name: smoke
script: tests/smoke.star
"#;
        let entry: TestEntry = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(entry.command.is_none());
        assert_eq!(
            entry.script.as_ref().map(|p| p.to_str().unwrap()),
            Some("tests/smoke.star")
        );
        assert!(entry.script_inline.is_none());
        let kind = entry.kind().unwrap();
        assert!(matches!(kind, TestKind::Script(_)), "expected Script, got {kind:?}");
    }

    #[test]
    fn parse_test_entry_script_inline_kind() {
        // Happy path: `script_inline:` field → TestKind::ScriptInline
        let yaml = "name: inline\nscript_inline: |\n  ocx_assert(True)\n";
        let entry: TestEntry = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(entry.command.is_none());
        assert!(entry.script.is_none());
        assert!(entry.script_inline.is_some());
        let kind = entry.kind().unwrap();
        assert!(
            matches!(kind, TestKind::ScriptInline(_)),
            "expected ScriptInline, got {kind:?}"
        );
    }

    #[test]
    fn validate_test_entry_none_set_produces_error() {
        // Reject: no kind field set at all.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        let relevant: Vec<_> = errors.iter().filter(|e| e.contains("none set")).collect();
        assert!(
            !relevant.is_empty(),
            "Expected 'none set' error for entry with no kind, got: {errors:?}"
        );
        assert!(
            relevant[0].contains("version"),
            "Error must mention the entry name 'version': {relevant:?}"
        );
    }

    #[test]
    fn validate_test_entry_multiple_set_produces_error() {
        // Reject: two kind fields set simultaneously.
        let yaml = format!(
            r#"{base}
tests:
  - name: multi
    command: shfmt --version
    script: tests/smoke.star
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        let relevant: Vec<_> = errors.iter().filter(|e| e.contains("set")).collect();
        assert!(
            !relevant.is_empty(),
            "Expected 'N set' error for entry with two kinds, got: {errors:?}"
        );
        assert!(
            relevant[0].contains("multi"),
            "Error must mention the entry name 'multi': {relevant:?}"
        );
        // Message must include a count (not zero)
        assert!(relevant[0].contains("2 set"), "Error must state '2 set': {relevant:?}");
    }

    #[test]
    fn validate_test_entry_exactly_one_passes() {
        // Happy path through validate(): single command entry should not add
        // any kind-related errors.
        let yaml = format!(
            r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        let kind_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.contains("command|script|script_inline"))
            .collect();
        assert!(
            kind_errors.is_empty(),
            "Single-command entry must not produce kind errors: {errors:?}"
        );
    }

    // ── announce ──────────────────────────────────────────────────────────

    #[test]
    fn announce_block_round_trips_and_defaults_the_index_repo() {
        let yaml = format!(
            r#"{base}
announce:
  package: bazelbuild/bazelisk
  fork: ocx-contrib/index
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let announce = spec.announce.as_ref().expect("announce block parsed");
        assert_eq!(announce.package, "bazelbuild/bazelisk");
        assert_eq!(announce.fork, "ocx-contrib/index");
        assert_eq!(announce.index_repo, DEFAULT_INDEX_REPO);
        assert!(
            spec.validate(Path::new("test.yml")).is_empty(),
            "valid announce block must not error"
        );
    }

    #[test]
    fn spec_without_announce_block_announces_nothing() {
        let spec: MirrorSpec = serde_yaml_ng::from_str(MINIMAL_BASE_YAML).unwrap();
        assert!(spec.announce.is_none(), "announce is opt-in");
    }

    #[test]
    fn validate_rejects_malformed_announce_package_with_a_named_error() {
        // A bare package name is the likely mistake — the index needs the
        // `<namespace>/<package>` pair, and the message has to say which
        // field is wrong rather than surface a serde shape mismatch.
        let yaml = format!(
            r#"{base}
announce:
  package: bazelisk
  fork: ocx-contrib/index
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("announce.package") && e.contains("<namespace>/<package>")),
            "malformed package must produce a named field error: {errors:?}"
        );
    }

    #[test]
    fn validate_rejects_malformed_announce_fork_and_index_repo() {
        let yaml = format!(
            r#"{base}
announce:
  package: bazelbuild/bazelisk
  fork: https://github.com/ocx-contrib/index
  index_repo: index
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("announce.fork")),
            "URL paste into fork must error: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("announce.index_repo")),
            "bare repo name must error: {errors:?}"
        );
    }

    #[tokio::test]
    async fn load_spec_rejects_an_announce_cron_that_could_add_its_own_triggers() {
        // `announce.schedule` is spliced into the generated workflow's `on:`
        // block inside a single-quoted scalar, exactly as the other two cron
        // fields are. A value that closes that scalar adds a trigger of the
        // spec's choosing — and a scheduled announce opens index pull requests
        // for real. Reject before render, naming the field to go fix.
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
name: announce-cron-guard
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
announce:
  package: test/test
  fork: ocx-contrib/index
"#;
        let spec_path = dir.path().join("mirror.yml");

        std::fs::write(
            &spec_path,
            format!("{body}  schedule: \"0 4 * * 1'\\n  push:\\n    branches: [main]\\n#\"\n"),
        )
        .unwrap();
        match load_spec(&spec_path).await.expect_err("injected cron must be rejected") {
            MirrorError::SpecInvalid(errors) => assert!(
                errors.iter().any(|e| e.contains("announce.schedule")),
                "the error must name the field: {errors:?}"
            ),
            other => panic!("expected SpecInvalid, got: {other}"),
        }

        std::fs::write(&spec_path, format!("{body}  schedule: \"23 5 * * 2\"\n")).unwrap();
        let spec = load_spec(&spec_path).await.expect("a plain cron must still load");
        assert_eq!(
            spec.announce.expect("announce block parsed").schedule.as_deref(),
            Some("23 5 * * 2")
        );
    }
}
