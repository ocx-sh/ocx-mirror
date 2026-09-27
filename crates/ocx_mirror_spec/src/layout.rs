// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `publish.layout` — where one archive lands, below both `output:` and
//! `publish.base_url` — `publish.dist`, where the two manifest documents
//! land, and `publish.installers`, where the patched installers land.
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
/// [`DestinationTemplate`](crate::destination::DestinationTemplate),
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
    /// A `{…}` naming something other than the placeholders this template
    /// knows; `known` spells them out for the message.
    UnknownPlaceholder { name: String, known: &'static str },
    /// A template that must vary per document but names no placeholder at
    /// all — every snapshot would render to one path.
    MissingPlaceholder { template: String, name: &'static str },
    /// A literal `{` that never closes.
    UnterminatedPlaceholder { template: String },
    /// A substituted value that is not a single safe path component.
    UnsafeValue { field: &'static str, value: String },
    /// A template whose own literals would leave `output:` — an absolute
    /// path, or a `..` / `.` segment.
    EscapingTemplate { template: String },
    /// A `publish.dist.path` that is empty or carries a `{` — it is a plain
    /// path, and there is nothing to substitute into it.
    NotAPlainPath { path: String },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlaceholder { name, known } => {
                write!(
                    f,
                    "unknown placeholder '{{{name}}}'; the known placeholders are {known}"
                )
            }
            Self::MissingPlaceholder { template, name } => write!(
                f,
                "template {template:?} names no '{{{name}}}' placeholder, so every document would render \
                 to the same path"
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
            Self::NotAPlainPath { path } => write!(
                f,
                "{path:?} is not a plain path — it must be non-empty and carry no placeholder; there is \
                 exactly one rolling manifest, so there is nothing to substitute"
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
                        known: "{version}, {tag}, {target}, {filename} and {channel}",
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

/// A parsed `publish.dist.snapshots` template.
///
/// One placeholder, `{sha256}` — the digest of the rendered manifest, which is
/// the whole point of the document: the path pins the bytes. It must appear,
/// because a template without it renders every snapshot to one path and the
/// second run silently overwrites the first pin.
#[derive(Debug, Clone)]
pub struct SnapshotTemplate {
    template: String,
}

impl SnapshotTemplate {
    const PLACEHOLDER: &'static str = "{sha256}";

    /// Parse a `publish.dist.snapshots` template.
    ///
    /// # Errors
    ///
    /// [`LayoutError::MissingPlaceholder`] when `{sha256}` is absent,
    /// [`LayoutError::UnknownPlaceholder`] for any other `{…}`,
    /// [`LayoutError::UnterminatedPlaceholder`] for a `{` that never closes,
    /// and [`LayoutError::EscapingTemplate`] for literals that leave `output:`.
    pub fn parse(template: &str) -> Result<SnapshotTemplate, LayoutError> {
        let mut rest = template;
        let mut seen = false;
        while let Some(open) = rest.find('{') {
            let after_open = &rest[open + 1..];
            let Some(close) = after_open.find('}') else {
                return Err(LayoutError::UnterminatedPlaceholder {
                    template: template.to_string(),
                });
            };
            match &after_open[..close] {
                "sha256" => seen = true,
                unknown => {
                    return Err(LayoutError::UnknownPlaceholder {
                        name: unknown.to_string(),
                        known: "{sha256}",
                    });
                }
            }
            rest = &after_open[close + 1..];
        }
        if !seen {
            return Err(LayoutError::MissingPlaceholder {
                template: template.to_string(),
                name: "sha256",
            });
        }
        check_literals(template)?;
        Ok(SnapshotTemplate {
            template: template.to_string(),
        })
    }

    /// Render the snapshot path for one manifest digest (lowercase hex, which
    /// is always a single safe path component).
    #[must_use]
    pub fn expand(&self, sha256_hex: &str) -> String {
        self.template.replace(Self::PLACEHOLDER, sha256_hex)
    }
}

/// Which `publish.installers` template is being parsed — each admits a
/// different placeholder set and requires a different one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerTemplateKind {
    /// `path` — the rolling copy, and `source` — the upstream URL.
    Rolling,
    /// `snapshots` — one copy per patched-bytes digest.
    Snapshot,
    /// `versions` — one copy per ocx release.
    Version,
}

impl InstallerTemplateKind {
    fn known(self) -> &'static str {
        match self {
            Self::Rolling => "{filename} and {shell}",
            Self::Snapshot => "{filename}, {shell} and {sha256}",
            Self::Version => "{filename}, {shell}, {version} and {tag}",
        }
    }
}

/// One piece of a parsed installer template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallerSegment {
    Literal(String),
    Filename,
    Shell,
    Sha256,
    Version,
    Tag,
}

/// The values an installer template substitutes. A placeholder the template's
/// kind does not admit is never read, so callers pass `""` for those.
#[derive(Debug, Clone, Copy)]
pub struct InstallerValues<'a> {
    pub filename: &'a str,
    pub shell: &'a str,
    pub sha256: &'a str,
    pub version: &'a str,
    pub tag: &'a str,
}

/// A parsed `publish.installers` template (`path`, `snapshots`, `versions`,
/// `source`).
///
/// Same closed-set doctrine as [`LayoutTemplate`]. Every template must name
/// `{filename}` or `{shell}`, or the five shells would render to one path; a
/// snapshot must name `{sha256}` and a version copy `{version}` or `{tag}`, for
/// the reason [`SnapshotTemplate`] requires its digest.
#[derive(Debug, Clone)]
pub struct InstallerTemplate {
    segments: Vec<InstallerSegment>,
}

impl InstallerTemplate {
    /// Parse one installer template of the given kind.
    ///
    /// Literals are containment-checked for every kind but
    /// [`InstallerTemplateKind::Rolling`] used as `source`, which is a URL —
    /// the caller passes `check_containment: false` there.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPlaceholder`] for a placeholder the kind does not
    /// admit, [`LayoutError::MissingPlaceholder`] for a missing required one,
    /// [`LayoutError::UnterminatedPlaceholder`], and
    /// [`LayoutError::EscapingTemplate`] for literals that leave `output:`.
    pub fn parse(
        template: &str,
        kind: InstallerTemplateKind,
        check_containment: bool,
    ) -> Result<InstallerTemplate, LayoutError> {
        let mut segments = Vec::new();
        let mut rest = template;

        while let Some(open) = rest.find('{') {
            let (literal, after_open) = (&rest[..open], &rest[open + 1..]);
            let Some(close) = after_open.find('}') else {
                return Err(LayoutError::UnterminatedPlaceholder {
                    template: template.to_string(),
                });
            };
            let placeholder = match (&after_open[..close], kind) {
                ("filename", _) => InstallerSegment::Filename,
                ("shell", _) => InstallerSegment::Shell,
                ("sha256", InstallerTemplateKind::Snapshot) => InstallerSegment::Sha256,
                ("version", InstallerTemplateKind::Version) => InstallerSegment::Version,
                ("tag", InstallerTemplateKind::Version) => InstallerSegment::Tag,
                (unknown, _) => {
                    return Err(LayoutError::UnknownPlaceholder {
                        name: unknown.to_string(),
                        known: kind.known(),
                    });
                }
            };
            if !literal.is_empty() {
                segments.push(InstallerSegment::Literal(literal.to_string()));
            }
            segments.push(placeholder);
            rest = &after_open[close + 1..];
        }
        if !rest.is_empty() {
            segments.push(InstallerSegment::Literal(rest.to_string()));
        }

        let names = |wanted: &[InstallerSegment]| segments.iter().any(|segment| wanted.contains(segment));
        let missing = |name| LayoutError::MissingPlaceholder {
            template: template.to_string(),
            name,
        };
        if !names(&[InstallerSegment::Filename, InstallerSegment::Shell]) {
            // Renders as '{filename}' or '{shell}' inside the message's braces.
            return Err(missing("filename}' or '{shell"));
        }
        match kind {
            InstallerTemplateKind::Snapshot if !names(&[InstallerSegment::Sha256]) => {
                return Err(missing("sha256"));
            }
            InstallerTemplateKind::Version if !names(&[InstallerSegment::Version, InstallerSegment::Tag]) => {
                return Err(missing("version}' or '{tag"));
            }
            _ => {}
        }

        if check_containment {
            check_literals(template)?;
        }
        Ok(InstallerTemplate { segments })
    }

    /// Render the template.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnsafeValue`] when a substituted value is not a single
    /// safe path component — `version` and `tag` come off a foreign manifest.
    pub fn expand(&self, values: &InstallerValues<'_>) -> Result<String, LayoutError> {
        let mut out = String::new();
        for segment in &self.segments {
            let (field, value) = match segment {
                InstallerSegment::Literal(literal) => {
                    out.push_str(literal);
                    continue;
                }
                InstallerSegment::Filename => ("filename", values.filename),
                InstallerSegment::Shell => ("shell", values.shell),
                InstallerSegment::Sha256 => ("sha256", values.sha256),
                InstallerSegment::Version => ("version", values.version),
                InstallerSegment::Tag => ("tag", values.tag),
            };
            check_path_component(field, value)?;
            out.push_str(value);
        }
        Ok(out)
    }
}

/// Validate `publish.dist.path` — a plain relative path, not a template.
///
/// There is exactly one rolling manifest, so it has nothing to substitute; a
/// `{` is refused so an operator who writes `{filename}` out of habit gets an
/// error rather than a file literally named that.
///
/// # Errors
///
/// [`LayoutError::NotAPlainPath`] for an empty path or one carrying a `{`,
/// [`LayoutError::EscapingTemplate`] for literals that leave `output:`.
pub fn check_plain_path(path: &str) -> Result<(), LayoutError> {
    if path.is_empty() || path.contains(['{', '}']) {
        return Err(LayoutError::NotAPlainPath { path: path.to_string() });
    }
    check_literals(path)
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
