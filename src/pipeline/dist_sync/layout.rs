// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `publish.layout` — where one archive lands, below both `output:` and
//! `publish.base_url`.
//!
//! The same rendered path is the file written on disk *and* the tail of the
//! `url` stamped into the mirrored manifest, so the two can never disagree
//! about where a byte lives.
//!
//! This is the containment boundary for the run. Every value substituted here
//! — `tag`, `filename`, `target` — comes off a manifest fetched over the
//! network, so each is refused rather than normalised: a `filename` of
//! `../../etc/cron.d/pwn` is a directory traversal against `output:`, and an
//! empty one silently collapses a path segment.

use std::fmt;

/// A parsed `publish.layout` template.
///
/// Plain substitution over exactly five placeholders. No template engine:
/// five names do not justify a dependency, and a closed set is what lets an
/// unknown placeholder be an error instead of an empty string. Same shape as
/// [`DestinationTemplate`](crate::pipeline::registry_sync::destination::DestinationTemplate),
/// which makes the same argument for `registry sync`.
#[derive(Debug, Clone)]
pub struct LayoutTemplate {
    segments: Vec<Segment>,
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Version,
    Tag,
    Target,
    Filename,
    Channel,
}

impl Segment {
    /// The placeholder's spelling, for error messages that name the field a
    /// bad value arrived in.
    fn name(&self) -> &'static str {
        match self {
            Segment::Literal(_) => "literal",
            Segment::Version => "version",
            Segment::Tag => "tag",
            Segment::Target => "target",
            Segment::Filename => "filename",
            Segment::Channel => "channel",
        }
    }
}

/// The five substitutable values, one manifest row's worth.
#[derive(Debug, Clone, Copy)]
pub struct RowValues<'a> {
    pub version: &'a str,
    pub tag: &'a str,
    pub target: &'a str,
    pub filename: &'a str,
    pub channel: &'a str,
}

/// Why a template or a row was refused.
///
/// Every message quotes the offending value with `{:?}` rather than bare
/// (CWE-117): these strings come straight off a foreign manifest, and one
/// holding a newline forges log lines in the CI output an operator reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutError {
    /// A `{…}` naming something other than the five known placeholders.
    UnknownPlaceholder { name: String },
    /// A literal `{` that never closes.
    UnterminatedPlaceholder { template: String },
    /// A substituted value that is not a single safe path component.
    UnsafeValue { field: &'static str, value: String },
    /// A template whose own literals would leave `output:` — an absolute
    /// path, or a `..` / `.` segment.
    EscapingTemplate { template: String },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlaceholder { name } => write!(
                f,
                "unknown placeholder '{{{name}}}'; the known placeholders are {{version}}, {{tag}}, \
                 {{target}}, {{filename}} and {{channel}}"
            ),
            Self::UnterminatedPlaceholder { template } => {
                write!(f, "unterminated '{{' in layout template {template:?}")
            }
            Self::UnsafeValue { field, value } => write!(
                f,
                "manifest row field '{field}' is {value:?}, which is not a single safe path component \
                 — empty, '.', '..', or a value carrying a path separator is refused, never repaired"
            ),
            Self::EscapingTemplate { template } => write!(
                f,
                "layout template {template:?} would write outside the output directory — it must be \
                 relative and carry no '.' or '..' segment"
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

impl LayoutTemplate {
    /// Parse a `publish.layout` template.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPlaceholder`] naming the offending placeholder,
    /// or [`LayoutError::UnterminatedPlaceholder`] for a `{` that opens no
    /// known placeholder.
    pub fn parse(template: &str) -> Result<LayoutTemplate, LayoutError> {
        let mut segments = Vec::new();
        let mut rest = template;

        while let Some(open) = rest.find('{') {
            let (literal, after_open) = (&rest[..open], &rest[open + 1..]);
            let Some(close) = after_open.find('}') else {
                return Err(LayoutError::UnterminatedPlaceholder {
                    template: template.to_string(),
                });
            };
            let placeholder = match &after_open[..close] {
                "version" => Segment::Version,
                "tag" => Segment::Tag,
                "target" => Segment::Target,
                "filename" => Segment::Filename,
                "channel" => Segment::Channel,
                unknown => {
                    return Err(LayoutError::UnknownPlaceholder {
                        name: unknown.to_string(),
                    });
                }
            };
            if !literal.is_empty() {
                segments.push(Segment::Literal(literal.to_string()));
            }
            segments.push(placeholder);
            rest = &after_open[close + 1..];
        }

        if !rest.is_empty() {
            segments.push(Segment::Literal(rest.to_string()));
        }

        check_literals(template)?;
        Ok(LayoutTemplate { segments })
    }

    /// Render one manifest row into its relative path.
    ///
    /// The result is always relative and always contained: only the
    /// template's own literals may contribute a `/`, because every
    /// substituted value is checked to be a single safe path component first.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnsafeValue`] naming the manifest field whose value
    /// would have escaped `output:` or collapsed a segment.
    pub fn expand(&self, row: &RowValues<'_>) -> Result<String, LayoutError> {
        let mut out = String::new();
        for segment in &self.segments {
            let value = match segment {
                Segment::Literal(literal) => {
                    out.push_str(literal);
                    continue;
                }
                Segment::Version => row.version,
                Segment::Tag => row.tag,
                Segment::Target => row.target,
                Segment::Filename => row.filename,
                Segment::Channel => row.channel,
            };
            check_path_component(segment.name(), value)?;
            out.push_str(value);
        }
        Ok(out)
    }
}

/// Refuse a template whose own literals leave `output:`.
///
/// The substituted values are guarded per-value by
/// [`check_path_component`], but the literals between them are not — and
/// `Path::join` discards its base when handed an absolute path, so a
/// `layout` of `/etc/{filename}` would write outside `output:` entirely, and
/// `../{filename}` would climb out of it.
///
/// This is operator-authored configuration rather than foreign data, so it is
/// outside the project's threat model — but this module states containment as
/// its contract, and an unenforced invariant is the kind that stops being true.
fn check_literals(template: &str) -> Result<(), LayoutError> {
    let escapes = template.starts_with('/')
        || template.starts_with('\\')
        || template
            .split(['/', '\\'])
            .any(|segment| segment == ".." || segment == ".");

    if escapes {
        return Err(LayoutError::EscapingTemplate {
            template: template.to_string(),
        });
    }
    Ok(())
}

/// Whether `value` is usable as exactly one path segment.
///
/// Refuses the empty string, `.`, `..`, and anything carrying `/`, `\` or a
/// NUL. `\` counts because the emitted tree is written on whatever platform
/// the mirror runs on, and a Windows runner treats it as a separator.
fn check_path_component(field: &'static str, value: &str) -> Result<(), LayoutError> {
    let unsafe_value = value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0');

    if unsafe_value {
        return Err(LayoutError::UnsafeValue {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "layout/tests.rs"]
mod tests;
