// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Exit-code classification for `source.type: pypi` failures.
//!
//! Lives beside [`MirrorError`] rather than in the source adapter so the
//! adapter never names the mirror's error type (adr_bazel_crate_split.md
//! § C1: the source crate sits below the error crate).

use super::MirrorError;

/// Classifies an error surfaced by `source::pypi::list_versions` into the
/// right [`MirrorError`] variant.
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
    // `crate::pylock::classify_error`).
    if is_not_found {
        MirrorError::PypiError(format!("{context}: {err:#}"))
    } else {
        MirrorError::SourceError(format!("{context}: {err:#}"))
    }
}

#[cfg(test)]
#[path = "pypi/tests.rs"]
mod tests;
