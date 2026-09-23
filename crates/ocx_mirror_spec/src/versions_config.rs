// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::fmt;
use std::path::Path;

use ocx_package::version::Version;
use serde::de;
use serde::{Deserialize, Deserializer, Serialize};

use crate::ValueSource;
use ocx_mirror_error::MirrorError;

/// Controls the order in which non-mirrored versions are selected when
/// `new_per_run` caps the batch size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillOrder {
    /// Prioritise the most recent versions first (default). New mirrors get
    /// the latest releases immediately; older versions trickle in over
    /// subsequent runs.
    #[default]
    NewestFirst,
    /// Start from the oldest non-mirrored version and work forward. Useful
    /// when chronological completeness matters more than freshness.
    OldestFirst,
}

impl fmt::Display for BackfillOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewestFirst => write!(f, "newest_first"),
            Self::OldestFirst => write!(f, "oldest_first"),
        }
    }
}

/// One edge of the version window: a literal version, or one resolved at run
/// time from a moving vendor pointer.
///
/// Two spellings, because the shorthand is what nearly every spec writes:
///
/// ```yaml
/// versions:
///   min: "1.0.0"              # inclusive, unchanged
///   max: "3.0.0"              # exclusive, unchanged
/// versions:
///   max:
///     version: "3.0.0"        # or {url: …} / {generator: {…}}
///     inclusive: true         # required here — the object form states its own
/// ```
///
/// `inclusive` has no default in the object form on purpose: an operator who
/// reached for the long spelling is changing the edge's behaviour, and
/// inheriting a default they did not write is how a ceiling ends up one
/// release off.
#[derive(Debug)]
pub struct Bound {
    /// Where the edge's version text comes from.
    pub version: ValueSource,
    /// Whether a candidate equal to the edge is inside the window.
    pub inclusive: bool,
}

/// The map spelling of a [`Bound`]. `inclusive` carries no `#[serde(default)]`
/// — a map missing it is ``missing field `inclusive` ``, which is the whole
/// point of spelling the edge out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundMap {
    version: ValueSource,
    inclusive: bool,
}

/// `min:` and `max:` share one type but not one shorthand default. A bare
/// string keeps exactly the meaning it had before the object form existed:
/// `min` inclusive, `max` exclusive. The object form states its own, so the
/// two fields name their own deserializer rather than sharing one
/// `Deserialize` impl that could not tell which edge it is reading.
fn min_bound<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Bound>, D::Error> {
    deserializer.deserialize_any(BoundVisitor {
        shorthand_inclusive: true,
    })
}

fn max_bound<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Bound>, D::Error> {
    deserializer.deserialize_any(BoundVisitor {
        shorthand_inclusive: false,
    })
}

struct BoundVisitor {
    shorthand_inclusive: bool,
}

impl<'de> de::Visitor<'de> for BoundVisitor {
    type Value = Option<Bound>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a version string, or a map with `version` and `inclusive`")
    }

    /// An explicit `min:` with no value is no bound at all — what
    /// `Option<String>` did before this type existed.
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Some(Bound {
            version: ValueSource::Literal(value.to_string()),
            inclusive: self.shorthand_inclusive,
        }))
    }

    fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        let raw = BoundMap::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(Some(Bound {
            version: raw.version,
            inclusive: raw.inclusive,
        }))
    }
}

/// The global version window, the rate limiter, and the generated workflow's
/// schedule.
///
/// `deny_unknown_fields` is load-bearing, not tidiness: without it a spec
/// written with an unknown key — `max_from:`, or an `inclusive:` one level too
/// high, which the object form makes a live typo — mirrors every release while
/// looking correct.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionsConfig {
    #[serde(default, deserialize_with = "min_bound")]
    pub min: Option<Bound>,
    #[serde(default, deserialize_with = "max_bound")]
    pub max: Option<Bound>,
    pub new_per_run: Option<usize>,
    #[serde(default)]
    pub backfill: BackfillOrder,
    /// Cron schedule expression (e.g. `"0 */6 * * *"`) used to generate a
    /// `schedule:` trigger in the rendered GHA workflow. Omitting this field
    /// produces a workflow with only `workflow_dispatch` + `push:` triggers.
    pub poll_interval: Option<String>,
}

impl VersionsConfig {
    pub fn validate(&self, errors: &mut Vec<String>) {
        if let Some(min) = &self.min {
            validate_bound(min, "versions.min", errors);
        }
        if let Some(max) = &self.max {
            validate_bound(max, "versions.max", errors);
        }
        if let Some(cron) = &self.poll_interval {
            super::validate_cron("versions.poll_interval", cron, errors);
        }
    }
}

/// One edge's validation, both edges' rules.
fn validate_bound(bound: &Bound, field: &str, errors: &mut Vec<String>) {
    bound.version.validate(&format!("{field}.version"), errors);
    // A written bound is still checked where it is written; a fetched or
    // generated one has no value yet and is checked at resolve time.
    if let ValueSource::Literal(literal) = &bound.version
        && Version::parse(literal).is_none()
    {
        errors.push(format!("{field}: invalid version '{literal}'"));
    }
}

/// The version window a run actually filtered by, after resolving both edges.
///
/// Reported in `plan.json` because a moving vendor pointer makes the spec alone
/// insufficient to reproduce a run. An unset edge emits no key; its inclusivity
/// flag is always present and carries the shorthand default, so a plan written
/// before the object form existed — or by hand — still reads correctly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
pub struct ResolvedBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default = "inclusive_min_default")]
    pub min_inclusive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
    #[serde(default)]
    pub max_inclusive: bool,
    /// Where each edge came from. Not serialized: `plan.json`'s shape is the
    /// four fields above, and the plain plan renderer is the only reader.
    #[serde(skip)]
    pub min_origin: BoundOrigin,
    #[serde(skip)]
    pub max_origin: BoundOrigin,
}

fn inclusive_min_default() -> bool {
    true
}

/// `Default` is hand-written, not derived: `min_inclusive` defaults to `true`,
/// and a derived `false` would silently narrow the window of every spec that
/// reaches the no-`versions:` path.
impl Default for ResolvedBounds {
    fn default() -> Self {
        Self {
            min: None,
            min_inclusive: true,
            max: None,
            max_inclusive: false,
            min_origin: BoundOrigin::default(),
            max_origin: BoundOrigin::default(),
        }
    }
}

impl ResolvedBounds {
    /// Whether `candidate` falls inside the window.
    pub fn admits(&self, candidate: &str) -> bool {
        crate::filter::within_bounds_ex(
            candidate,
            self.min.as_deref(),
            self.min_inclusive,
            self.max.as_deref(),
            self.max_inclusive,
        )
    }
}

/// Where a resolved edge's value came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BoundOrigin {
    /// Written in the spec.
    #[default]
    Spec,
    /// Fetched from a URL.
    Url,
    /// Produced by a generator command.
    Generator,
}

impl fmt::Display for BoundOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spec => write!(f, "spec"),
            Self::Url => write!(f, "url"),
            Self::Generator => write!(f, "generator"),
        }
    }
}

/// Resolve `versions:`'s two edges once per run.
///
/// Fail-closed: an edge the resolver cannot produce, or cannot read back as a
/// version, aborts the command. Falling back to "unbounded" would mirror every
/// release the pointer exists to hold back — the one outcome #78 rules out.
///
/// Called once per crawling command (`sync`, `check`, `pipeline plan`), never
/// at spec load: `validate` would otherwise fetch the value too.
///
/// # Errors
///
/// [`MirrorError::SourceError`] when either edge cannot be fetched, run, read
/// as UTF-8, or related to itself by [`crate::filter::version_cmp`].
pub async fn resolve_version_bounds(
    versions: Option<&VersionsConfig>,
    spec_dir: &Path,
) -> Result<ResolvedBounds, MirrorError> {
    let Some(config) = versions else {
        return Ok(ResolvedBounds::default());
    };
    let mut resolved = ResolvedBounds::default();

    if let Some(min) = &config.min {
        let (value, origin) = resolve_bound(min, "versions.min", spec_dir).await?;
        resolved.min = Some(value);
        resolved.min_inclusive = min.inclusive;
        resolved.min_origin = origin;
    }
    if let Some(max) = &config.max {
        let (value, origin) = resolve_bound(max, "versions.max", spec_dir).await?;
        resolved.max = Some(value);
        resolved.max_inclusive = max.inclusive;
        resolved.max_origin = origin;
    }

    log_resolved(&resolved);
    Ok(resolved)
}

async fn resolve_bound(bound: &Bound, field: &str, spec_dir: &Path) -> Result<(String, BoundOrigin), MirrorError> {
    let value = bound.version.resolve(&format!("{field}.version"), spec_dir).await?;

    // Checked against the comparator that will use it, not the stricter
    // `Version::parse` a literal bound gets: a pointer serving a four-component
    // PEP 440 release (`1.16.0.0`) is a bound `within_bounds` handles
    // perfectly, and refusing it would fail a run that works.
    if crate::filter::version_cmp(&value, &value).is_none() {
        return Err(MirrorError::SourceError(format!(
            "{field} resolved to '{}', which is not a version",
            display_preview(&value)
        )));
    }

    let origin = match &bound.version {
        ValueSource::Literal(_) => BoundOrigin::Spec,
        ValueSource::Url(_) => BoundOrigin::Url,
        ValueSource::Generator(_) => BoundOrigin::Generator,
    };
    Ok((value, origin))
}

/// Characters of a resolved value a diagnostic may repeat back.
const PREVIEW_CHARS: usize = 80;

/// A resolved value as it is safe to put in a log line.
///
/// What a pointer served is foreign bytes bounded only by the 64 KiB fetch
/// ceiling: a vendor serving an HTML error page puts the whole page in the CI
/// log, and an ANSI escape in it rewrites the terminal reading that log. Both
/// are log-hygiene failures, so the value is truncated and its control
/// characters escaped before it is interpolated.
fn display_preview(value: &str) -> String {
    let mut preview = String::new();
    let mut chars = value.chars();
    for c in chars.by_ref().take(PREVIEW_CHARS) {
        match c.is_control() {
            true => preview.extend(c.escape_default()),
            false => preview.push(c),
        }
    }
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

/// One line per edge that was not written in the spec. A literal bound is
/// already in the file, and logging it every run is noise.
fn log_resolved(resolved: &ResolvedBounds) {
    for (field, value, inclusive, origin) in [
        (
            "versions.min",
            &resolved.min,
            resolved.min_inclusive,
            resolved.min_origin,
        ),
        (
            "versions.max",
            &resolved.max,
            resolved.max_inclusive,
            resolved.max_origin,
        ),
    ] {
        if origin == BoundOrigin::Spec {
            continue;
        }
        if let Some(value) = value {
            let edge = if inclusive { "inclusive" } else { "exclusive" };
            log::info!("{field} resolved to {value} ({edge}, from {origin})");
        }
    }
}

#[cfg(test)]
mod tests {
    use ocx_exit::ExitCode;

    use super::*;

    #[test]
    fn the_default_window_admits_everything() {
        // The value every spec without a `versions:` block is filtered by.
        // Reds if someone re-derives `Default` and flips `min_inclusive`.
        let bounds = ResolvedBounds::default();
        assert!(bounds.admits("0.0.1"));
        assert!(bounds.admits("99.0.0"));
        assert!(bounds.min_inclusive);
        assert!(!bounds.max_inclusive);
    }

    fn parse(yaml: &str) -> VersionsConfig {
        serde_yaml_ng::from_str(yaml).expect("the versions block must parse")
    }

    fn parse_err(yaml: &str) -> String {
        serde_yaml_ng::from_str::<VersionsConfig>(yaml)
            .expect_err("the versions block must be rejected")
            .to_string()
    }

    /// The whole spec, so a refusal is asserted with the exit code an operator
    /// actually sees rather than a bare serde string.
    async fn load_rejection(versions_block: &str) -> MirrorError {
        let dir = tempfile::tempdir().expect("temporary directory");
        let spec_path = dir.path().join("mirror.yml");
        std::fs::write(
            &spec_path,
            format!(
                r#"
name: test-tool
target:
  registry: ocx.sh
  repository: test-tool
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
{versions_block}
"#
            ),
        )
        .expect("spec is writable");

        crate::load_spec(&spec_path)
            .await
            .err()
            .unwrap_or_else(|| panic!("the document must be rejected:\n{versions_block}"))
    }

    fn literal(bound: &Option<Bound>) -> &str {
        match bound {
            Some(Bound {
                version: ValueSource::Literal(value),
                ..
            }) => value,
            other => panic!("expected a literal bound, got {other:?}"),
        }
    }

    fn generated(command: &[&str]) -> Bound {
        Bound {
            version: ValueSource::Generator(ocx_mirror_source::generator::GeneratorConfig {
                command: command.iter().map(|arg| (*arg).to_string()).collect(),
                working_directory: None,
                timeout_seconds: 10,
            }),
            inclusive: true,
        }
    }

    fn errors_for(config: &VersionsConfig) -> Vec<String> {
        let mut errors = Vec::new();
        config.validate(&mut errors);
        errors
    }

    #[test]
    fn the_min_shorthand_is_a_literal_and_inclusive() {
        let config = parse("min: \"1.0.0\"\n");
        assert_eq!(literal(&config.min), "1.0.0");
        assert!(config.min.as_ref().expect("a min bound").inclusive);
    }

    #[test]
    fn the_max_shorthand_is_a_literal_and_exclusive() {
        let config = parse("max: \"3.0.0\"\n");
        assert_eq!(literal(&config.max), "3.0.0");
        assert!(!config.max.as_ref().expect("a max bound").inclusive);
    }

    #[test]
    fn the_object_form_carries_the_inclusivity_it_states() {
        let config = parse("max:\n  version: \"3.0.0\"\n  inclusive: true\n");
        assert_eq!(literal(&config.max), "3.0.0");
        assert!(config.max.as_ref().expect("a max bound").inclusive);
    }

    #[test]
    fn an_object_bound_reads_a_url_and_a_generator() {
        let config = parse(
            "min:\n  version:\n    url: https://example.com/floor\n  inclusive: false\n\
             max:\n  version:\n    generator:\n      command: [\"sh\", \"-c\", \"echo 3.0.0\"]\n  inclusive: true\n",
        );
        assert!(matches!(
            config.min.as_ref().map(|b| &b.version),
            Some(ValueSource::Url(url)) if url == "https://example.com/floor"
        ));
        assert!(matches!(
            config.max.as_ref().map(|b| &b.version),
            Some(ValueSource::Generator(_))
        ));
    }

    #[tokio::test]
    async fn the_object_form_requires_inclusive_on_either_edge() {
        for edge in ["min", "max"] {
            let error = load_rejection(&format!("versions:\n  {edge}:\n    version: \"3.0.0\"\n")).await;
            assert_eq!(
                error.kind_exit_code(),
                ExitCode::DataError,
                "a missing required key is malformed data (65): {error}"
            );
            let rendered = error.to_string();
            assert!(
                rendered.contains("missing field `inclusive`"),
                "the message must name the missing key: {rendered}"
            );
        }
    }

    #[test]
    fn an_object_bound_without_a_version_names_the_missing_key() {
        // `max: {url: …}` — the nesting forgotten. An untagged enum would have
        // said only "did not match any variant".
        let rendered = parse_err("max:\n  url: https://example.com/stable\n  inclusive: true\n");
        assert!(rendered.contains("unknown field `url`"), "got: {rendered}");
    }

    #[test]
    fn an_object_bound_missing_version_entirely_names_it() {
        let rendered = parse_err("max:\n  inclusive: true\n");
        assert!(rendered.contains("missing field `version`"), "got: {rendered}");
    }

    #[tokio::test]
    async fn inclusive_as_a_sibling_of_the_shorthand_is_refused_by_name() {
        // The typo `deny_unknown_fields` on `versions:` exists to catch: the
        // shorthand plus an `inclusive:` one level too high would otherwise be
        // dropped silently and mirror every release.
        let error = load_rejection("versions:\n  max: \"3.0.0\"\n  inclusive: true\n").await;
        assert_eq!(error.kind_exit_code(), ExitCode::DataError, "{error}");
        assert!(error.to_string().contains("unknown field `inclusive`"), "got: {error}");
    }

    #[test]
    fn an_explicit_null_bound_is_no_bound() {
        let config = parse("min:\nnew_per_run: 5\n");
        assert!(config.min.is_none(), "an empty `min:` is no floor");
        assert_eq!(config.new_per_run, Some(5));
    }

    #[test]
    fn a_literal_bound_that_is_not_a_version_is_still_refused() {
        // Byte-identical to the message the shorthand produced before the
        // object form existed.
        for (yaml, field) in [
            ("min: \"not-a-version\"\n", "versions.min"),
            ("max: \"not-a-version\"\n", "versions.max"),
            (
                "max:\n  version: \"not-a-version\"\n  inclusive: true\n",
                "versions.max",
            ),
        ] {
            let errors = errors_for(&parse(yaml));
            assert_eq!(
                errors,
                vec![format!("{field}: invalid version 'not-a-version'")],
                "got: {errors:?}"
            );
        }
    }

    #[test]
    fn a_resolvable_bound_is_not_version_checked_at_validation() {
        // The value does not exist yet; only its shape is checkable here.
        let config = parse("max:\n  version:\n    url: https://example.com/stable\n  inclusive: true\n");
        assert!(errors_for(&config).is_empty());
    }

    #[tokio::test]
    async fn resolve_version_bounds_carries_literals_through() {
        let config = parse("min: \"1.0.0\"\nmax: \"3.0.0\"\n");
        let bounds = resolve_version_bounds(Some(&config), Path::new("."))
            .await
            .expect("literal edges resolve");

        assert_eq!(bounds.min.as_deref(), Some("1.0.0"));
        assert_eq!(bounds.max.as_deref(), Some("3.0.0"));
        assert!(bounds.min_inclusive, "the min shorthand stays inclusive");
        assert!(!bounds.max_inclusive, "the max shorthand stays exclusive");
        assert_eq!(bounds.min_origin, BoundOrigin::Spec);
        assert_eq!(bounds.max_origin, BoundOrigin::Spec);
        assert!(bounds.admits("1.0.0"));
        assert!(!bounds.admits("3.0.0"));
        assert!(!bounds.admits("0.9.0"));
        assert!(bounds.admits("2.9.9"));
    }

    #[tokio::test]
    async fn resolve_version_bounds_stamps_the_generator_origin_and_edge() {
        let config = VersionsConfig {
            max: Some(generated(&["sh", "-c", "echo 2.0.0"])),
            ..Default::default()
        };
        let bounds = resolve_version_bounds(Some(&config), Path::new("."))
            .await
            .expect("the generator resolves");

        assert_eq!(bounds.max.as_deref(), Some("2.0.0"));
        assert!(bounds.max_inclusive, "the object form's `inclusive: true` travels");
        assert_eq!(bounds.max_origin, BoundOrigin::Generator);
        assert!(bounds.admits("2.0.0"), "an inclusive ceiling keeps its boundary");
        assert!(!bounds.admits("2.0.1"));
    }

    #[tokio::test]
    async fn resolve_version_bounds_refuses_a_generated_value_that_is_not_a_version() {
        for edge in ["min", "max"] {
            let bound = generated(&["sh", "-c", "echo not-a-version"]);
            let config = match edge {
                "min" => VersionsConfig {
                    min: Some(bound),
                    ..Default::default()
                },
                _ => VersionsConfig {
                    max: Some(bound),
                    ..Default::default()
                },
            };

            let error = resolve_version_bounds(Some(&config), Path::new("."))
                .await
                .expect_err("a non-version must abort the run");
            assert_eq!(
                error.kind_exit_code(),
                ExitCode::Unavailable,
                "a failed resolve is 69: {error}"
            );
            let rendered = error.to_string();
            assert!(
                rendered.contains(&format!(
                    "versions.{edge} resolved to 'not-a-version', which is not a version"
                )),
                "got: {rendered}"
            );
        }
    }

    /// A bound whose value is written inline — the shortest way to hand
    /// `resolve_bound` an exact byte sequence a pointer could have served.
    fn literal_bound(value: &str) -> Bound {
        Bound {
            version: ValueSource::Literal(value.to_string()),
            inclusive: true,
        }
    }

    #[tokio::test]
    async fn a_bad_bound_does_not_echo_a_whole_page_into_the_log() {
        // The value is whatever the vendor pointer served, bounded only by the
        // 64 KiB fetch ceiling. Untruncated, a maintenance page reaches the CI
        // log in full.
        let body = format!("<html><body>{}</body></html>", "A".repeat(64 * 1024));
        let config = VersionsConfig {
            min: Some(literal_bound(&body)),
            ..Default::default()
        };

        let rendered = resolve_version_bounds(Some(&config), Path::new("."))
            .await
            .expect_err("an HTML page is not a version")
            .to_string();
        assert!(
            rendered.chars().count() < 200,
            "the message must be bounded, got {} chars",
            rendered.chars().count()
        );
        assert!(rendered.contains('…'), "and must say it truncated: {rendered}");
        assert!(rendered.contains("which is not a version"), "{rendered}");
    }

    #[tokio::test]
    async fn a_bad_bound_escapes_control_characters() {
        // An ANSI escape in a durable log rewrites the terminal reading it.
        let config = VersionsConfig {
            max: Some(literal_bound("3.0.0\u{1b}[2J\u{1b}[1;31mowned\u{7}")),
            ..Default::default()
        };

        let rendered = resolve_version_bounds(Some(&config), Path::new("."))
            .await
            .expect_err("an escape sequence is not a version")
            .to_string();
        assert!(
            !rendered.chars().any(char::is_control),
            "no raw control character may survive: {rendered:?}"
        );
        assert!(rendered.contains("\\u{1b}"), "escaped, not dropped: {rendered}");
    }

    #[tokio::test]
    async fn resolve_version_bounds_accepts_a_four_segment_pep440_bound() {
        // Comparator parity: `Version::parse` rejects `1.16.0.0`, the PEP 440
        // parser `within_bounds` falls back to does not. Reds if someone
        // "tidies" the check back to `Version::parse`.
        let config = VersionsConfig {
            min: Some(generated(&["sh", "-c", "echo 1.16.0.0"])),
            ..Default::default()
        };
        let bounds = resolve_version_bounds(Some(&config), Path::new("."))
            .await
            .expect("a four-component PEP 440 release is a usable bound");

        assert_eq!(bounds.min.as_deref(), Some("1.16.0.0"));
        assert!(bounds.admits("1.16.1"));
        assert!(!bounds.admits("1.15.0"));
    }

    #[tokio::test]
    async fn a_failing_generator_aborts_rather_than_widening_the_window() {
        let config = VersionsConfig {
            max: Some(generated(&["false"])),
            ..Default::default()
        };
        let error = resolve_version_bounds(Some(&config), Path::new("."))
            .await
            .expect_err("a failed generator must abort the run");
        assert_eq!(error.kind_exit_code(), ExitCode::Unavailable, "{error}");
        assert!(
            error.to_string().contains("versions.max.version:"),
            "the message must name the edge: {error}"
        );
    }

    #[tokio::test]
    async fn no_versions_block_resolves_to_no_bounds() {
        let bounds = resolve_version_bounds(None, Path::new("."))
            .await
            .expect("no block never fails");
        assert_eq!(bounds.min, None);
        assert_eq!(bounds.max, None);
        assert!(bounds.admits("0.0.1"));
        assert!(bounds.admits("99.0.0"));
    }
}
