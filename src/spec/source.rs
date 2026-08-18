// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_TAG_PATTERN: &str = r"^v?(?P<version>\d+\.\d+\.\d+)(?:-(?P<prerelease>[0-9a-zA-Z]+))?$";

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    GithubRelease {
        owner: String,
        repo: String,
        #[serde(default = "default_tag_pattern")]
        tag_pattern: String,
    },
    UrlIndex(UrlIndexSource),
    /// A PEP 751 `pylock.toml` committed alongside the mirror spec.
    ///
    /// Unlike `GithubRelease`/`UrlIndex` (many upstream versions discovered
    /// per run), a lock resolves one version: the project version recorded in
    /// the lock itself. Wheel selection uses variant constraint fields
    /// (`spec/variant.rs`) instead of asset regex patterns.
    Pylock {
        /// Path to the committed `pylock.toml`, relative to the spec directory.
        path: String,
        /// PEP 503 name of the application package to resolve inside the lock.
        ///
        /// Defaults to the mirror's `name`. Set it when the mirror must carry a
        /// different name than the locked package — e.g. a `pycowsay-musl`
        /// mirror (distinct target repo + workflow file) that resolves the
        /// `pycowsay` package from a shared lock for the alpine/musl leg.
        #[serde(default)]
        package: Option<String>,
    },
    /// PyPI-discovered Python application: versions come from a Simple
    /// Repository API index (PEP 503 / PEP 691) and a PEP 751 lock is derived
    /// in-pipeline per version (see pipeline plan).
    Pypi {
        /// PEP 503 name of the PyPI package. Defaults to the mirror's `name`.
        #[serde(default)]
        package: Option<String>,
        /// Simple Repository API index bases, highest priority first.
        ///
        /// A list rather than one key because a corporate deployment routinely
        /// resolves an internal index *and* a public one — `pip` spells that
        /// `--extra-index-url`, `uv` spells it `[[tool.uv.index]]`, and npm
        /// spells it per-scope registries, so the shape has to be plural to
        /// survive the sibling ecosystems.
        ///
        /// Empty (the default) means pypi.org. There is deliberately **no**
        /// credential key here: a `mirror.yml` is community-contributed, and a
        /// spec able to name an environment variable is a spec able to
        /// exfiltrate it. Credentials resolve from the request's own host
        /// (see `crate::auth`).
        #[serde(default)]
        indexes: Vec<PackageIndex>,
    },
}

/// One upstream package index.
///
/// A struct rather than a bare string so the sibling ecosystems can add their
/// own routing key without a second spelling of the same list — npm's
/// per-scope registries being the next one.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PackageIndex {
    /// The index base URL, exactly as the operator writes it for `pip` or
    /// `uv` — `https://nexus.corp.example/repository/pypi-remote/simple`.
    ///
    /// No suffix is appended to it: every vendor lays its Simple API out
    /// differently (`/api/pypi/<repo>/simple` on Artifactory,
    /// `/repository/<repo>/simple` on Nexus), and guessing one is what
    /// confined the previous implementation to pypi.org.
    pub url: String,
}

impl Source {
    /// The PEP 503 application-package name a `pylock`/`pypi` source resolves:
    /// the source's `package` override when set, otherwise the mirror `name`.
    /// Returns `spec_name` unchanged for other source types (never consulted
    /// there).
    pub fn pylock_app_name<'a>(&'a self, spec_name: &'a str) -> &'a str {
        match self {
            Source::Pylock {
                package: Some(package), ..
            }
            | Source::Pypi {
                package: Some(package), ..
            } => package,
            _ => spec_name,
        }
    }

    /// The index bases a `pypi` source resolves against, highest priority
    /// first — the configured list, or pypi.org when the spec names none.
    ///
    /// One place owns the default so discovery and lock derivation cannot
    /// disagree about which index a spec meant.
    pub fn pypi_indexes(&self) -> Vec<String> {
        let Source::Pypi { indexes, .. } = self else {
            return Vec::new();
        };
        if indexes.is_empty() {
            return vec![crate::source::pypi::DEFAULT_INDEX.to_string()];
        }
        indexes.iter().map(|index| index.url.clone()).collect()
    }

    /// Whether this source resolves an application package into an env
    /// package (wheel selection via variant constraint fields, `python:`
    /// required) rather than per-platform archive/binary assets via regex
    /// patterns: `pylock` (committed lock) or `pypi` (index-discovered, lock
    /// derived in-pipeline).
    pub fn is_env(&self) -> bool {
        self.env_type_name().is_some()
    }

    /// The `source.type` discriminant string (`"pylock"`/`"pypi"`) for an env
    /// source, `None` for any other source. Used to name the concrete source
    /// type in validation errors that reject a field for env sources (e.g.
    /// `metadata:` — env metadata is composed from the lock, not configured).
    pub fn env_type_name(&self) -> Option<&'static str> {
        match self {
            Source::Pylock { .. } => Some("pylock"),
            Source::Pypi { .. } => Some("pypi"),
            _ => None,
        }
    }
}

/// The three modes of providing url_index data.
///
/// Exactly one of `url`, `versions`, or `generator` must be specified.
/// This is enforced by a custom `Deserialize` impl that rejects
/// missing fields, multiple fields, and unknown fields.
#[derive(Debug)]
pub enum UrlIndexSource {
    /// Fetch url_index JSON from a remote URL.
    Remote { url: String },
    /// Inline version->assets map directly in the mirror spec.
    Inline { versions: HashMap<String, UrlIndexVersion> },
    /// Run an external command that outputs url_index JSON to stdout.
    Generator { generator: GeneratorConfig },
}

/// Helper for validating exactly one url_index mode is specified.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UrlIndexSourceRaw {
    url: Option<String>,
    versions: Option<HashMap<String, UrlIndexVersion>>,
    generator: Option<GeneratorConfig>,
}

impl<'de> Deserialize<'de> for UrlIndexSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = UrlIndexSourceRaw::deserialize(deserializer)?;
        match (raw.url, raw.versions, raw.generator) {
            (Some(url), None, None) => Ok(UrlIndexSource::Remote { url }),
            (None, Some(versions), None) => Ok(UrlIndexSource::Inline { versions }),
            (None, None, Some(generator)) => Ok(UrlIndexSource::Generator { generator }),
            (None, None, None) => Err(serde::de::Error::custom(
                "url_index source requires one of: url, versions, generator",
            )),
            _ => Err(serde::de::Error::custom(
                "url_index source must have exactly one of: url, versions, generator",
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UrlIndexVersion {
    #[serde(default)]
    pub prerelease: bool,
    pub assets: HashMap<String, String>,
}

/// Configuration for an external generator command.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratorConfig {
    /// Command to execute. First element is the executable, rest are arguments.
    /// Must output url_index JSON to stdout.
    pub command: Vec<String>,
    /// Working directory for the command.
    /// Relative paths are resolved from the mirror spec directory.
    /// Default: the spec directory.
    pub working_directory: Option<String>,
    /// Timeout in seconds for the generator command. Default: 60.
    #[serde(default = "default_generator_timeout")]
    pub timeout_seconds: u64,
}

impl GeneratorConfig {
    /// Resolve the working directory for this generator.
    /// Default: spec directory. If `working_directory` is set, resolve relative to spec dir.
    pub fn resolve_working_directory(&self, spec_dir: &Path) -> PathBuf {
        match &self.working_directory {
            Some(wd) => spec_dir.join(wd),
            None => spec_dir.to_path_buf(),
        }
    }
}

fn default_generator_timeout() -> u64 {
    60
}

fn default_tag_pattern() -> String {
    DEFAULT_TAG_PATTERN.to_string()
}

impl Source {
    pub fn validate(&self, errors: &mut Vec<String>) {
        match self {
            Source::GithubRelease { tag_pattern, .. } => match regex::Regex::new(tag_pattern) {
                Ok(re) => {
                    if re.capture_names().flatten().all(|n| n != "version") {
                        errors
                            .push("source.tag_pattern must contain a named capture group (?P<version>...)".to_string());
                    }
                }
                Err(e) => {
                    errors.push(format!("source.tag_pattern is not a valid regex: {e}"));
                }
            },
            Source::UrlIndex(UrlIndexSource::Generator { generator }) => {
                if generator.command.is_empty() {
                    errors.push("source.generator.command must be a non-empty list".to_string());
                }
            }
            Source::UrlIndex(_) => {}
            Source::Pylock { path, .. } => {
                if path.trim().is_empty() {
                    errors.push("source.path must not be empty".to_string());
                }
            }
            Source::Pypi { package, indexes } => {
                if let Some(package) = package
                    && package.trim().is_empty()
                {
                    errors.push("source.package must not be empty".to_string());
                }
                for (position, index) in indexes.iter().enumerate() {
                    match url::Url::parse(&index.url) {
                        Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                            // Userinfo in a committed spec is a credential in a
                            // committed spec, whatever the intent — and this
                            // file is contributed, not operator-authored. It
                            // would also reach the `uv` subprocess argv, where
                            // `/proc/<pid>/cmdline` is world-readable.
                            // Refused, never stripped: an operator who wrote it
                            // meant it, and silently dropping it would resolve
                            // against an index they did not name.
                            if !parsed.username().is_empty() || parsed.password().is_some() {
                                errors.push(format!(
                                    "source.indexes[{position}].url must not embed credentials; \
                                     set OCX_AUTH_<slug>_TOKEN in the environment instead"
                                ));
                            }
                        }
                        Ok(_) => errors.push(format!(
                            "source.indexes[{position}].url '{}' must be an http(s) URL",
                            index.url
                        )),
                        Err(e) => errors.push(format!(
                            "source.indexes[{position}].url '{}' is not a valid URL: {e}",
                            index.url
                        )),
                    }
                }
            }
        }
    }
}
