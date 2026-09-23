// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! A spec value written inline, fetched over HTTP, or produced by a command.
//!
//! The `url:` / `generator:` pair is deliberately the same shape
//! `source.url_index` already uses ([`crate::Source`]), so an operator
//! who can write one can write the other. It resolves to **trimmed text**;
//! what that text must *mean* is the caller's rule — for `versions.min` /
//! `versions.max` it is "something [`crate::filter::version_cmp`] can relate".

use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde::de;

use ocx_mirror_error::MirrorError;
use ocx_mirror_source::generator::GeneratorConfig;

/// Body cap for a `url:` value. The values this carries are tens of bytes; the
/// cap exists so a hostile or misconfigured endpoint cannot stream a run out
/// of memory, not to leave room for growth.
const VALUE_FETCH_CEILING: usize = 64 * 1024;

/// Bound on one value fetch. [`ocx_mirror_http::builder`] sets only a connect
/// timeout, so a server that accepts the connection and then stalls would
/// otherwise hang the run indefinitely.
const VALUE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a single spec value comes from.
// ponytail: the 64 KiB cap and the 30s request timeout are fixed, not
// configurable — a spec knob here buys nothing a version string needs.
#[derive(Debug)]
pub enum ValueSource {
    /// Written in the spec.
    Literal(String),
    /// Fetched from a URL, once per run.
    Url(String),
    /// The stdout of a command, run once per run.
    Generator(GeneratorConfig),
}

/// The map spelling of a [`ValueSource`].
///
/// Deserialized on its own rather than as an `#[serde(untagged)]` variant: an
/// untagged enum reports every failure as "data did not match any variant",
/// swallowing the `unknown field` diagnostic that tells an operator which key
/// they misspelled.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueSourceMap {
    url: Option<String>,
    generator: Option<GeneratorConfig>,
}

impl<'de> Deserialize<'de> for ValueSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueSourceVisitor)
    }
}

struct ValueSourceVisitor;

impl<'de> de::Visitor<'de> for ValueSourceVisitor {
    type Value = ValueSource;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a literal string, or a map with exactly one of `url` or `generator`")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(ValueSource::Literal(value.to_string()))
    }

    fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        let raw = ValueSourceMap::deserialize(de::value::MapAccessDeserializer::new(map))?;
        // Same exactly-one-of shape, and the same wording, as
        // `UrlIndexSource`'s `url`/`versions`/`generator` match.
        match (raw.url, raw.generator) {
            (Some(url), None) => Ok(ValueSource::Url(url)),
            (None, Some(generator)) => Ok(ValueSource::Generator(generator)),
            (None, None) => Err(de::Error::custom("a value source requires one of: url, generator")),
            _ => Err(de::Error::custom(
                "a value source must have exactly one of: url, generator",
            )),
        }
    }
}

impl ValueSource {
    /// Check what can be checked before the value exists.
    ///
    /// `field` is the caller's dotted path (`versions.max.version`), so one
    /// validator serves every edge that carries a value source.
    pub(crate) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        match self {
            // The caller validates what the text must mean; there is nothing
            // structural to check here.
            Self::Literal(_) => {}
            Self::Url(url) => match url::Url::parse(url) {
                Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                    // Userinfo in a committed spec is a credential in a
                    // committed spec, whatever the intent — same rule and same
                    // reasoning as `source.indexes[].url`. Refused, never
                    // stripped: an operator who wrote it meant it.
                    if !parsed.username().is_empty() || parsed.password().is_some() {
                        errors.push(format!(
                            "{field}.url must not embed credentials; \
                             set OCX_AUTH_<slug>_TOKEN in the environment instead"
                        ));
                    }
                }
                Ok(_) => errors.push(format!("{field}.url '{url}' must be an http(s) URL")),
                Err(e) => errors.push(format!("{field}.url '{url}' is not a valid URL: {e}")),
            },
            Self::Generator(generator) => {
                if generator.command.is_empty() {
                    errors.push(format!("{field}.generator.command must be a non-empty list"));
                }
            }
        }
    }

    /// Produce the value, trimmed.
    ///
    /// Fail-closed: every failure is a [`MirrorError::SourceError`] (exit 69),
    /// the classification `sync` already stamps on every `from_remote` /
    /// `from_generator` failure. There is no fallback value — a bound that
    /// cannot be resolved must not silently widen the window it exists to
    /// narrow.
    ///
    /// # Errors
    ///
    /// [`MirrorError::SourceError`] when the URL cannot be fetched or read,
    /// the generator cannot be run, the bytes are not UTF-8, or the value is
    /// empty after trimming. [`MirrorError::ExecutionFailed`] when the TLS
    /// backend cannot be built.
    pub(crate) async fn resolve(&self, field: &str, spec_dir: &Path) -> Result<String, MirrorError> {
        let bytes = match self {
            Self::Literal(value) => return Ok(value.trim().to_string()),
            Self::Url(url) => fetch(url).await.map_err(|error| name_the_field(field, error))?,
            Self::Generator(generator) => ocx_mirror_source::generator::run(generator, spec_dir)
                .await
                .map_err(|error| MirrorError::SourceError(format!("{field}: {error:#}")))?,
        };

        let text = String::from_utf8(bytes)
            .map_err(|error| MirrorError::SourceError(format!("{field} is not valid UTF-8: {error}")))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(MirrorError::SourceError(format!("{field} resolved to an empty value")));
        }
        Ok(trimmed.to_string())
    }
}

/// Name the edge in a message the transport wrote without one.
///
/// A spec can resolve both edges, so "cannot fetch <url>" alone leaves the
/// operator correlating URLs to work out which bound failed. Only
/// [`MirrorError::SourceError`] is rewritten: a client that cannot be built is
/// not this edge's fault.
fn name_the_field(field: &str, error: MirrorError) -> MirrorError {
    match error {
        MirrorError::SourceError(message) => MirrorError::SourceError(format!("{field}: {message}")),
        other => other,
    }
}

/// Fetch one value over HTTP, bounded in size and in time.
///
/// Follows `dist_sync::fetch_manifest`, not `url_index::from_remote`: that one
/// has no auth, no cap and no request timeout, and copying its shape is a
/// Block-tier defect this crate has already recorded once.
async fn fetch(url: &str) -> Result<Vec<u8>, MirrorError> {
    let parsed =
        url::Url::parse(url).map_err(|error| MirrorError::SourceError(format!("cannot parse {url}: {error}")))?;

    // `builder()`, not `client()`: the request timeout is layered on top, the
    // way `dist_sync::build_client` does. Starting from
    // `reqwest::Client::builder()` is what silently drops the operator's roots.
    let client = ocx_mirror_http::builder()
        .timeout(VALUE_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("cannot build an HTTP client: {error}")]))?;

    // Host-keyed like every other read leg: a credential is attached only when
    // the operator keyed one to the host the spec actually names.
    let mut request = client.get(parsed.clone());
    if let Some(credential) = ocx_mirror_http::auth::resolve(&parsed)? {
        request = credential.apply(request);
    }

    let mut response = request
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        // `{:#}` over an `anyhow` wrapper: reqwest's own `Display` stops at
        // "error sending request for url (…)", which on a restricted network
        // is the entire content of the message.
        .map_err(|error| MirrorError::SourceError(format!("cannot fetch {url}: {:#}", anyhow::Error::new(error))))?;

    ocx_mirror_http::read_capped(&mut response, url, VALUE_FETCH_CEILING)
        .await
        .map_err(MirrorError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> ValueSource {
        serde_yaml_ng::from_str(yaml).expect("a value source must parse")
    }

    fn parse_err(yaml: &str) -> String {
        serde_yaml_ng::from_str::<ValueSource>(yaml)
            .expect_err("the value source must be rejected")
            .to_string()
    }

    fn errors_for(source: &ValueSource) -> Vec<String> {
        let mut errors = Vec::new();
        source.validate("versions.max.version", &mut errors);
        errors
    }

    /// A generator spelled as the shell does, so the tests drive real
    /// subprocesses instead of a mock — the shape `url_index`'s own generator
    /// tests use.
    fn generator(command: &[&str], timeout_seconds: u64) -> ValueSource {
        ValueSource::Generator(GeneratorConfig {
            command: command.iter().map(|arg| (*arg).to_string()).collect(),
            working_directory: None,
            timeout_seconds,
        })
    }

    async fn resolve_err(source: &ValueSource) -> MirrorError {
        source
            .resolve("versions.max.version", Path::new("."))
            .await
            .expect_err("the value must not resolve")
    }

    #[test]
    fn a_bare_string_is_a_literal() {
        assert!(matches!(parse("\"3.0.0\""), ValueSource::Literal(v) if v == "3.0.0"));
    }

    #[test]
    fn a_map_with_both_keys_names_both() {
        let rendered = parse_err("url: https://example.com/v\ngenerator:\n  command: [\"echo\", \"1\"]\n");
        assert!(
            rendered.contains("must have exactly one of: url, generator"),
            "got: {rendered}"
        );
    }

    #[test]
    fn a_map_with_neither_key_names_both() {
        let rendered = parse_err("{}");
        assert!(rendered.contains("requires one of: url, generator"), "got: {rendered}");
    }

    #[test]
    fn an_unknown_key_under_a_value_source_is_refused_by_name() {
        // The diagnostic an untagged enum would have swallowed.
        let rendered = parse_err("ulr: https://example.com/v\n");
        assert!(rendered.contains("unknown field `ulr`"), "got: {rendered}");
    }

    #[tokio::test]
    async fn a_generator_value_is_trimmed() {
        let source = generator(&["sh", "-c", "printf '3.0.0\\n'"], 10);
        let value = source
            .resolve("versions.max.version", Path::new("."))
            .await
            .expect("the generator resolves");
        assert_eq!(value, "3.0.0");
    }

    #[tokio::test]
    async fn a_generator_that_prints_nothing_is_refused() {
        let error = resolve_err(&generator(&["true"], 10)).await;
        assert!(error.to_string().contains("produced no output"), "got: {error}");
    }

    #[tokio::test]
    async fn a_generator_that_prints_only_whitespace_is_refused() {
        let error = resolve_err(&generator(&["sh", "-c", "echo '  '"], 10)).await;
        assert!(error.to_string().contains("resolved to an empty value"), "got: {error}");
    }

    #[tokio::test]
    async fn a_generator_timeout_is_refused() {
        let error = resolve_err(&generator(&["sleep", "10"], 1)).await;
        assert!(error.to_string().contains("timed out"), "got: {error}");
    }

    #[tokio::test]
    async fn a_generator_that_cannot_spawn_is_refused() {
        let error = resolve_err(&generator(&["nonexistent-command-xyz-12345"], 10)).await;
        assert!(error.to_string().contains("failed to run generator"), "got: {error}");
    }

    #[tokio::test]
    async fn an_empty_generator_command_is_refused_before_any_spawn() {
        let error = resolve_err(&generator(&[], 10)).await;
        assert!(error.to_string().contains("generator command is empty"), "got: {error}");
    }

    #[test]
    fn a_non_http_url_is_refused_at_validation() {
        let errors = errors_for(&ValueSource::Url("ftp://example.com/stable".to_string()));
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(
            errors[0].contains("versions.max.version.url 'ftp://example.com/stable' must be an http(s) URL"),
            "got: {errors:?}"
        );
    }

    #[test]
    fn a_url_with_userinfo_is_refused() {
        let errors = errors_for(&ValueSource::Url("https://user:pass@example.com/stable".to_string()));
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(errors[0].contains("must not embed credentials"), "got: {errors:?}");
    }

    #[test]
    fn an_unparseable_url_names_the_parse_failure() {
        let errors = errors_for(&ValueSource::Url("not a url".to_string()));
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(errors[0].contains("is not a valid URL"), "got: {errors:?}");
    }

    #[test]
    fn an_empty_generator_command_is_refused_at_validation() {
        let errors = errors_for(&generator(&[], 60));
        assert_eq!(
            errors,
            vec!["versions.max.version.generator.command must be a non-empty list".to_string()]
        );
    }

    #[test]
    fn a_literal_carries_no_structural_rules() {
        assert!(errors_for(&ValueSource::Literal("not-a-version".to_string())).is_empty());
    }

    #[test]
    fn the_validation_field_label_is_the_callers() {
        // A copy-paste of the `max` path would hardcode `versions.max`; the
        // lower edge has to name itself.
        let mut errors = Vec::new();
        ValueSource::Url("ftp://example.com/floor".to_string()).validate("versions.min.version", &mut errors);
        assert!(errors[0].starts_with("versions.min.version.url"), "got: {errors:?}");
    }
}
