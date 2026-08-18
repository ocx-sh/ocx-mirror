// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `source.type: pypi` adapter: discovers upstream versions through the
//! **Simple Repository API** — PEP 503 (HTML) and its PEP 691 JSON form.
//!
//! # Why not the `/pypi/<project>/json` endpoint
//!
//! That endpoint is Warehouse's own, not a packaging standard. Artifactory,
//! Nexus, CodeArtifact, Azure Artifacts and Google Artifact Registry serve the
//! Simple API and nothing else, so a mirror that discovers through the JSON API
//! works against pypi.org and fails against every index a corporate deployment
//! actually runs. The Simple API is what `pip` and `uv` themselves resolve
//! against, which makes it the only form an index is obliged to speak.
//!
//! # Versions from filenames
//!
//! PEP 700 adds a `versions` key to the JSON form, but only at `api-version`
//! 1.1+, and the HTML form has no equivalent at all. Both forms always list
//! *files*, and a file's version is in its name — so versions are derived from
//! filenames in every case rather than through two code paths that would
//! disagree on any index serving 1.0. It also keeps the yank rule exact: a
//! version survives only while some file of it is un-yanked (PEP 592), which
//! `versions` alone cannot express.
//!
//! Unlike `pylock` (a single committed lock resolving exactly one version), a
//! `pypi` source lists every release the index still serves. Per-version lock
//! derivation is a separate pipeline stage, so `VersionInfo::assets` stays
//! empty exactly like `source::pylock` (env sources resolve wheels later, from
//! a derived lock, not asset regex matching — see [`crate::spec::Source::is_env`]).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::LazyLock;

use ocx_lib::log;
use regex::Regex;
use serde::Deserialize;

use super::VersionInfo;
use crate::error::MirrorError;

/// Default Simple API base, used when a spec lists no index.
pub const DEFAULT_INDEX: &str = "https://pypi.org/simple";

/// Content negotiation per PEP 691: the JSON serialization first, then the
/// two HTML forms. An index that understands none of it answers `text/html`
/// anyway, which [`parse_html`] handles.
const SIMPLE_ACCEPT: &str = "application/vnd.pypi.simple.v1+json, \
                             application/vnd.pypi.simple.v1+html;q=0.2, \
                             text/html;q=0.01";

/// One file of a project, in the PEP 691 JSON serialization. Only the two
/// fields discovery needs are read; hashes, metadata pointers and
/// `requires-python` belong to resolution, which is `uv`'s job.
#[derive(Debug, Deserialize)]
struct SimpleFile {
    filename: String,
    #[serde(default)]
    yanked: Yanked,
}

/// PEP 592 in its PEP 691 encoding: `false`, `true`, or a reason string that
/// is itself the yank marker. A bare string means yanked whatever it says.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Yanked {
    Flag(bool),
    /// The reason text is never read — its *presence* is the yank marker.
    Reason(#[allow(dead_code)] String),
}

impl Yanked {
    fn is_yanked(&self) -> bool {
        match self {
            Self::Flag(flag) => *flag,
            Self::Reason(_) => true,
        }
    }
}

impl Default for Yanked {
    fn default() -> Self {
        Self::Flag(false)
    }
}

/// A project page in the PEP 691 JSON serialization.
#[derive(Debug, Deserialize)]
struct SimpleProject {
    #[serde(default)]
    files: Vec<SimpleFile>,
}

/// Lists upstream versions for a package: every version with at least one
/// non-yanked file, from the first index that serves the project.
///
/// Indexes are tried in order and the **first one that has the project wins** —
/// uv's `first-index` strategy, and the reason it is the default there: merging
/// candidates across a private and a public index is how a dependency-confusion
/// attack lands. An index that answers 404 does not have the project, so the
/// next one is tried; every other failure aborts, because a 500 or a refused
/// connection means "unknown", not "absent".
///
/// # Errors
///
/// The last 404 when no index has the project — [`classify_error`] maps that to
/// [`MirrorError::PypiError`] (exit 65, malformed input). Any transport or
/// parse failure surfaces as itself and maps to [`MirrorError::SourceError`].
pub async fn list_versions(package: &str, indexes: &[String]) -> anyhow::Result<Vec<VersionInfo>> {
    let client = crate::http::client()?;
    let normalized = ocx_python::normalize_package_name(package);

    let mut last_absent = None;
    for index in indexes {
        let url = project_url(index, &normalized);
        log::debug!("Querying simple index {url}");

        let mut request = client.get(&url).header(reqwest::header::ACCEPT, SIMPLE_ACCEPT);
        if let Some(credential) = crate::auth::resolve(&url::Url::parse(&url)?)? {
            request = credential.apply(request);
        }

        let response = request.send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            // Kept as an error value, not a message: `classify_error` reads the
            // `reqwest::Error` status out of the chain to decide the exit code.
            last_absent = Some(response.error_for_status().expect_err("404 is an error status"));
            continue;
        }
        let response = response.error_for_status()?;

        let is_json = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("json"));
        let body = response.text().await?;

        let files = if is_json {
            let project: SimpleProject = serde_json::from_str(&body)?;
            project
                .files
                .into_iter()
                .map(|file| (file.filename, file.yanked.is_yanked()))
                .collect()
        } else {
            parse_html(&body)
        };

        return Ok(versions_from_files(files));
    }

    Err(last_absent
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("no index configured to query for '{package}'")))
}

/// `{base}/{project}/` — the PEP 503 project URL. The base is the index root
/// as the operator wrote it (`https://nexus.corp.example/repository/pypi/simple`),
/// never a suffix this function invents: every vendor lays that path out
/// differently, and guessing one is how the JSON-API assumption got here.
fn project_url(index: &str, normalized_package: &str) -> String {
    format!("{}/{normalized_package}/", index.trim_end_matches('/'))
}

/// Matches one PEP 503 anchor: its attributes and its text.
///
/// ponytail: a regex, not an HTML parser. A PEP 503 page is machine-generated
/// and specified as "a valid HTML5 page with a single anchor element per file",
/// so the only structure that matters is `<a …>filename</a>`. Swap in a real
/// parser if an index ever ships anchors this cannot read — the JSON form is
/// preferred by content negotiation anyway, so this is the fallback path.
static ANCHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<a\b([^>]*)>(.*?)</a>").expect("anchor regex compiles"));

/// Extracts `(filename, yanked)` from a PEP 503 HTML project page.
///
/// The filename is the anchor's text, per the specification — not the `href`,
/// which may carry a fragment, a mirror-rewritten path or a query string.
fn parse_html(body: &str) -> Vec<(String, bool)> {
    ANCHOR
        .captures_iter(body)
        .filter_map(|capture| {
            let attributes = capture.get(1).map_or("", |m| m.as_str());
            let filename = capture.get(2)?.as_str().trim();
            if filename.is_empty() {
                return None;
            }
            Some((filename.to_string(), attributes.contains("data-yanked")))
        })
        .collect()
}

/// Folds a file listing into the version list, dropping every version whose
/// files are all yanked (PEP 592) and every version string that would be
/// dangerous downstream.
fn versions_from_files(files: Vec<(String, bool)>) -> Vec<VersionInfo> {
    // version -> (any file un-yanked, is_prerelease)
    let mut seen: HashMap<String, (bool, bool)> = HashMap::new();

    for (filename, yanked) in files {
        let Some(parsed) = ocx_python::uv_distribution_filename::DistFilename::try_from_normalized_filename(&filename)
        else {
            // Not a wheel or sdist name this parser recognizes — a signature,
            // a checksum sidecar, or a file type the mirror cannot use anyway.
            continue;
        };
        let version = parsed.version().to_string();

        // Trust boundary: filenames are attacker-controlled when an index is
        // hostile or compromised. The version string is later piped verbatim
        // into `uv pip compile -` stdin as `{package}=={version}` — a newline
        // smuggles a second requirement line ("evil @ https://attacker/…")
        // that resolves, hash-self-verifies against the attacker's own bytes,
        // and publishes under the legit tag; the same string also joins a
        // filesystem path for the derived lock. Reject any version carrying
        // whitespace, a control char, or a path separator BEFORE it reaches
        // either sink. This is orthogonal to PEP 440 parseability: a
        // weird-but-safe scheme still mirrors — only dangerous characters are
        // rejected, never "doesn't parse".
        if let Some(bad) = version
            .chars()
            .find(|ch| ch.is_whitespace() || ch.is_control() || *ch == '/' || *ch == '\\')
        {
            log::warn!(
                "dropping PyPI release with unsafe version string {version:?} (contains {bad:?}); \
                 a hostile index cannot smuggle a requirement line or path traversal through it"
            );
            continue;
        }
        // Same boundary, different sinks: `.`/`..` are real parent/self path
        // components once `work_dir.join(version)` runs, and a leading `-`
        // reaches `--version <V>` in the generated workflow where clap would
        // read it as a flag.
        if version.is_empty() || version == "." || version == ".." || version.starts_with('-') {
            log::warn!(
                "dropping PyPI release with unsafe version string {version:?}; \
                 it would act as a path component or CLI flag downstream"
            );
            continue;
        }

        let is_prerelease = parsed.version().any_prerelease();
        match seen.entry(version) {
            Entry::Occupied(mut entry) => entry.get_mut().0 |= !yanked,
            Entry::Vacant(entry) => {
                entry.insert((!yanked, is_prerelease));
            }
        }
    }

    seen.into_iter()
        .filter(|(_, (any_usable, _))| *any_usable)
        .map(|(version, (_, is_prerelease))| VersionInfo {
            version,
            assets: HashMap::new(),
            is_prerelease,
        })
        .collect()
}

/// Classifies an error surfaced by [`list_versions`] into the right
/// [`MirrorError`] variant.
///
/// A 404 from every configured index means the package name does not exist
/// there — malformed input, same exit class as `SpecInvalid`/`PylockError`
/// (65). Any other failure (connection refused, timeout, 5xx, malformed body)
/// is a genuinely unavailable source, `MirrorError::SourceError` (69).
pub fn classify_error(context: &str, err: anyhow::Error) -> MirrorError {
    let is_not_found = err
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|e| e.status() == Some(reqwest::StatusCode::NOT_FOUND));
    // `{err:#}` (alternate format) walks the full source chain instead of
    // just the outermost context string (same rationale as
    // `source::pylock::classify_error`).
    if is_not_found {
        MirrorError::PypiError(format!("{context}: {err:#}"))
    } else {
        MirrorError::SourceError(format!("{context}: {err:#}"))
    }
}

#[cfg(test)]
#[path = "pypi/tests.rs"]
mod tests;
