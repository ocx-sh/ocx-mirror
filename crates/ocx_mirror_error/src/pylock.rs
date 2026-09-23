// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Exit-code classification for `source.type: pylock` failures.
//!
//! Lives beside [`MirrorError`] rather than in the source adapter so the
//! adapter never names the mirror's error type (adr_bazel_crate_split.md
//! § C1: the source crate sits below the error crate).

use ocx_python::LockError;

use super::MirrorError;

/// Classifies an error surfaced by `source::pylock::load` or
/// `source::pylock::list_versions` into the right [`MirrorError`] variant.
///
/// A lock-content problem — malformed TOML, a sdist-only package, a wheel
/// missing its hash (any [`LockError`] in the chain) — is malformed DATA, not
/// a transient resource, so it maps to [`MirrorError::PylockError`] (exit 65,
/// same class as `SpecInvalid`). Anything else (the file itself could not be
/// read) is a genuinely unavailable SOURCE and stays
/// [`MirrorError::SourceError`] (exit 69).
pub fn classify_error(context: &str, err: anyhow::Error) -> MirrorError {
    let is_lock_data_error = err.chain().any(|cause| cause.downcast_ref::<LockError>().is_some());
    // `{err:#}` (alternate format) walks the full source chain instead of
    // just the outermost context string — otherwise the actionable detail
    // (e.g. the offending package name from a `LockError`) never reaches the
    // printed message.
    if is_lock_data_error {
        MirrorError::PylockError(format!("{context}: {err:#}"))
    } else {
        MirrorError::SourceError(format!("{context}: {err:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocx_mirror_source::pylock::load;

    #[tokio::test]
    async fn classify_error_maps_lock_content_error_to_pylock_error() {
        // W2.6: a sdist-only package fails at parse (LockError::SdistOnly) —
        // malformed lock content, must classify as PylockError (exit 65),
        // not SourceError (exit 69).
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
lock-version = "1.0"

[[packages]]
name = "uwsgi"
version = "2.0.24"
"#;
        tokio::fs::write(dir.path().join("pylock.toml"), toml).await.unwrap();

        let err = load(dir.path(), "pylock.toml").await.unwrap_err();
        let mirror_err = classify_error("failed to load pylock source", err);
        assert!(matches!(mirror_err, MirrorError::PylockError(_)), "got: {mirror_err:?}");
        // Exact bytes: pinned before the crate split moves this module (E4).
        assert_eq!(
            mirror_err.to_string(),
            "pylock error: failed to load pylock source: failed to parse pylock.toml: package 'uwsgi' has no wheels (sdist-only)"
        );
        assert_eq!(mirror_err.kind_exit_code(), ocx_exit::ExitCode::DataError);
    }

    #[tokio::test]
    async fn classify_error_maps_missing_file_to_source_error() {
        // A genuinely unreadable file is an unavailable source, not bad data.
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path(), "missing.toml").await.unwrap_err();
        let mirror_err = classify_error("failed to load pylock source", err);
        assert!(matches!(mirror_err, MirrorError::SourceError(_)), "got: {mirror_err:?}");
        // Exact bytes: pinned before the crate split moves this module (E4).
        assert_eq!(
            mirror_err.to_string(),
            format!(
                "source error: failed to load pylock source: failed to read pylock file '{}': No such file or directory (os error 2)",
                dir.path().join("missing.toml").display()
            )
        );
        assert_eq!(mirror_err.kind_exit_code(), ocx_exit::ExitCode::Unavailable);
    }
}
