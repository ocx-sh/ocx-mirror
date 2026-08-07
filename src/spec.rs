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
#[path = "spec/tests.rs"]
mod tests;
