// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;

use serde::Deserialize;
use url::Url;

use ocx_mirror_error::MirrorError;
// Moved into the source crate beside their users (plan D-P4); re-exported so
// `spec::GeneratorConfig` / `spec::UrlIndexVersion` keep resolving.
pub use ocx_mirror_source::generator::GeneratorConfig;
pub use ocx_mirror_source::url_index::UrlIndexVersion;

const DEFAULT_TAG_PATTERN: &str = r"^v?(?P<version>\d+\.\d+\.\d+)(?:-(?P<prerelease>[0-9a-zA-Z]+))?$";

/// Where upstream versions come from.
///
/// `deny_unknown_fields` alongside the `type` tag (the `spec/dist.rs`
/// `Identity` precedent): a stray or misspelled key under `source:` used to be
/// ignored silently, which is how a spec can crawl the wrong thing for months.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    GithubRelease {
        owner: String,
        repo: String,
        #[serde(default = "default_tag_pattern")]
        tag_pattern: String,
        /// Download-host substitution — discovery stays on the GitHub API.
        #[serde(default)]
        url_rewrite: Option<UrlRewrite>,
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
        /// Declared on every variant so a `url_rewrite:` here is refused by
        /// name rather than by a generic "unknown field", which reads as
        /// "the feature does not exist". See [`Source::validate`].
        #[serde(default)]
        url_rewrite: Option<UrlRewrite>,
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
        /// (see `ocx_mirror_http::auth`).
        #[serde(default)]
        indexes: Vec<PackageIndex>,
        /// Refused for this source type — see [`Source::Pylock::url_rewrite`].
        #[serde(default)]
        url_rewrite: Option<UrlRewrite>,
    },
}

/// A prefix substitution applied to every upstream asset URL a source
/// produces. Discovery stays where it is; only the download host moves.
///
/// ```yaml
/// source:
///   type: github_release
///   owner: Kitware
///   repo: CMake
///   url_rewrite:
///     from: "https://github.com/"
///     to: "https://artifactory.example.com/artifactory/githubcom-remote/"
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UrlRewrite {
    /// Literal prefix to match against the URL's string form.
    pub from: String,
    /// Replacement for `from`.
    pub to: String,
}

/// The operator-set override for [`UrlRewrite`], spelled `<from>=<to>`.
pub const URL_REWRITE_ENV: &str = "OCX_MIRROR_URL_REWRITE";

impl UrlRewrite {
    /// The rewrite in force: [`URL_REWRITE_ENV`] if set, else the spec's own
    /// block.
    ///
    /// Operator-set, never spec-named: a `mirror.yml` is community-contributed
    /// and a spec able to name the environment is a spec able to read it
    /// (`ocx_mirror_http::auth`). The variable is the whole point of the feature — it
    /// keeps one spec byte-identical between `ocx-contrib` and an internal
    /// fork, which `extends:`' shallow top-level merge cannot express.
    /// Both halves come back in normalised WHATWG form: this is the one funnel
    /// [`apply`](Self::apply) is reached through, so normalising here covers
    /// the spec form and the environment form in one place.
    pub fn resolve(spec: Option<&Self>) -> Result<Option<Self>, MirrorError> {
        let Some(raw) = ocx_util::env::var(URL_REWRITE_ENV).filter(|value| !value.is_empty()) else {
            return Ok(spec.cloned().map(Self::normalized));
        };

        // The variable is named, never its value: this reader sits beside the
        // credential ladder and its message reaches a durable CI log.
        let refuse = |reason: String| {
            MirrorError::SpecUsageError(format!(
                "{URL_REWRITE_ENV} is malformed: {reason} (expected '<from>=<to>')"
            ))
        };
        let (from, to) = raw
            .split_once('=')
            .ok_or_else(|| refuse("no '=' separating <from> from <to>".to_string()))?;

        // Validated by the same rules as the spec form, before it is used.
        let rewrite = Self {
            from: from.to_string(),
            to: to.to_string(),
        };
        for (part, value) in rewrite.parts() {
            if let Some(reason) = Self::check(part, value) {
                return Err(refuse(reason));
            }
        }
        Ok(Some(rewrite.normalized()))
    }

    /// `url` with `from` replaced by `to`.
    ///
    /// A URL that does not carry the prefix comes back unchanged — a release
    /// routinely hosts one asset on a CDN the proxy does not front.
    ///
    /// Both halves are assumed normalised, which is what
    /// [`resolve`](Self::resolve) hands out: the prefix is matched against
    /// `url.as_str()`, itself always in WHATWG form, and `to` always carries a
    /// path so the concatenated remainder cannot reach back into the authority.
    ///
    /// A `SourceError` (69), not a usage error: the remainder is foreign data
    /// off an already-parsed upstream URL, so a failure here is the upstream's,
    /// not the operator's.
    pub fn apply(&self, url: &Url) -> Result<Url, MirrorError> {
        let Some(rest) = url.as_str().strip_prefix(&self.from) else {
            return Ok(url.clone());
        };
        Url::parse(&format!("{}{rest}", self.to)).map_err(|e| {
            MirrorError::SourceError(format!(
                "source.url_rewrite.to does not form a URL when applied to an upstream asset: {e}"
            ))
        })
    }

    /// Both halves in WHATWG form — the only form [`apply`](Self::apply) can
    /// work in, because it matches them against `Url::as_str()`.
    ///
    /// Normalised rather than refused: `https://GitHub.com/`,
    /// `HTTPS://github.com/` and `https://github.com:443/` all name the host
    /// the operator meant, and refusing them would trade a silent no-op for a
    /// failed run. Left non-normalised they validate clean and then match
    /// nothing, so every download goes to the origin and the proxy the feature
    /// exists to route through is bypassed without a diagnostic.
    ///
    /// It also terminates `to`'s authority — `https://proxy.internal` becomes
    /// `https://proxy.internal/` — so a crafted upstream URL whose remainder
    /// starts with `@` or `.` can no longer extend or replace the host of the
    /// concatenation.
    ///
    /// A half that does not parse is left as written; `check` is what refuses it.
    fn normalized(self) -> Self {
        let whatwg = |value: String| Url::parse(&value).map_or(value, |url| url.to_string());
        Self {
            from: whatwg(self.from),
            to: whatwg(self.to),
        }
    }

    fn parts(&self) -> [(&'static str, &str); 2] {
        [("from", &self.from), ("to", &self.to)]
    }

    /// The rule one half must satisfy, as a reason **carrying no value**: the
    /// environment form reports it verbatim, and one of the things it refuses
    /// is a credential.
    fn check(part: &'static str, value: &str) -> Option<String> {
        if value.trim().is_empty() {
            return Some(format!("{part} must not be empty"));
        }
        match Url::parse(value) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                // Refused, never stripped — the `source.indexes` rule, for the
                // same reason: an operator who wrote it meant it, and dropping
                // it silently would fetch from a host they did not name. Both
                // halves: `from` is never dialled, but the rule the project
                // states is that a credential must not live in a committed
                // spec at all, and `prescan`, `source.indexes[].url` and
                // `ValueSource::validate` all enforce exactly that.
                let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
                has_userinfo.then(|| {
                    format!("{part} must not embed credentials; set OCX_AUTH_<slug>_TOKEN in the environment instead")
                })
            }
            _ => Some(format!("{part} must be an http(s) URL prefix")),
        }
    }

    fn validate(&self, errors: &mut Vec<String>) {
        for (part, value) in self.parts() {
            if let Some(reason) = Self::check(part, value) {
                errors.push(format!("source.url_rewrite.{reason}"));
            }
        }
    }
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
            return vec![ocx_mirror_source::pypi::DEFAULT_INDEX.to_string()];
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

    /// The spec's own download-URL rewrite for this source, if any. Refused at
    /// validation for env sources, so a `Some` here is always archive-shaped.
    pub fn url_rewrite(&self) -> Option<&UrlRewrite> {
        match self {
            Source::GithubRelease { url_rewrite, .. }
            | Source::Pylock { url_rewrite, .. }
            | Source::Pypi { url_rewrite, .. } => url_rewrite.as_ref(),
            Source::UrlIndex(source) => source.url_rewrite.as_ref(),
        }
    }
}

/// A `url_index` source: where the document comes from, plus the fields every
/// source type shares.
#[derive(Debug)]
pub struct UrlIndexSource {
    pub mode: UrlIndexMode,
    pub url_rewrite: Option<UrlRewrite>,
}

/// The three modes of providing url_index data.
///
/// Exactly one of `url`, `versions`, or `generator` must be specified.
/// This is enforced by a custom `Deserialize` impl on [`UrlIndexSource`] that
/// rejects missing fields, multiple fields, and unknown fields.
#[derive(Debug)]
pub enum UrlIndexMode {
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
    #[serde(default)]
    url_rewrite: Option<UrlRewrite>,
}

impl<'de> Deserialize<'de> for UrlIndexSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = UrlIndexSourceRaw::deserialize(deserializer)?;
        let mode = match (raw.url, raw.versions, raw.generator) {
            (Some(url), None, None) => UrlIndexMode::Remote { url },
            (None, Some(versions), None) => UrlIndexMode::Inline { versions },
            (None, None, Some(generator)) => UrlIndexMode::Generator { generator },
            (None, None, None) => {
                return Err(serde::de::Error::custom(
                    "url_index source requires one of: url, versions, generator",
                ));
            }
            _ => {
                return Err(serde::de::Error::custom(
                    "url_index source must have exactly one of: url, versions, generator",
                ));
            }
        };
        Ok(UrlIndexSource {
            mode,
            url_rewrite: raw.url_rewrite,
        })
    }
}

fn default_tag_pattern() -> String {
    DEFAULT_TAG_PATTERN.to_string()
}

impl Source {
    pub fn validate(&self, errors: &mut Vec<String>) {
        // One arm for all four variants: env sources carry the field only so
        // the refusal can name it, and an archive source's block is checked by
        // the same rules the environment override is.
        match (self.url_rewrite(), self.env_type_name()) {
            (Some(_), Some(source_type)) => errors.push(format!(
                "source.url_rewrite: not supported for source.type '{source_type}' \
                 — a proxy index belongs in source.indexes"
            )),
            (Some(rewrite), None) => rewrite.validate(errors),
            (None, _) => {}
        }

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
            Source::UrlIndex(UrlIndexSource {
                mode: UrlIndexMode::Generator { generator },
                ..
            }) => {
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
            Source::Pypi { package, indexes, .. } => {
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
