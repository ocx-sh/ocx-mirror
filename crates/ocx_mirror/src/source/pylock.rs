// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `source.type: pylock` adapter: reads a committed `pylock.toml` and exposes
//! it as a single upstream [`VersionInfo`] — the app package's locked
//! version. Unlike `github_release`/`url_index` (many upstream versions
//! discovered per run), a lock resolves exactly one version, so this adapter
//! only extracts that version; the per-platform wheel URLs are resolved later
//! by the plan phase (`command/package/pipeline/plan.rs`), which needs the
//! full parsed [`Pylock`] to run `ocx_python::select_wheels` — see
//! `design_spec_ocx_python.md`, "Mirror integration".

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use ocx_lib::package::version::Version;
use ocx_python::{LockError, LockedPackage, Pylock};

use super::VersionInfo;
use crate::error::MirrorError;

/// Reads and parses the `pylock.toml` at `path` (resolved relative to
/// `spec_dir`).
///
/// # Errors
///
/// Returns an error when the file cannot be read or fails PEP 751 parsing.
/// Uses `.context()`/`.with_context()` rather than `anyhow::anyhow!(...)` so
/// the original `std::io::Error` / [`LockError`] stays in the chain —
/// [`classify_error`] downcasts it to tell a bad file from bad content.
pub async fn load(spec_dir: &Path, path: &str) -> anyhow::Result<Pylock> {
    let lock_path = spec_dir.join(path);
    let contents = tokio::fs::read_to_string(&lock_path)
        .await
        .with_context(|| format!("failed to read pylock file '{}'", lock_path.display()))?;
    ocx_python::parse_pylock(&contents).context("failed to parse pylock.toml")
}

/// Classifies an error surfaced by [`load`] or [`list_versions`] into the
/// right [`MirrorError`] variant.
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

/// Lists the single upstream version recorded in the lock: the pinned
/// version of the locked package matching `app_name` (PEP 503 normalized
/// against the mirror spec's `name`, e.g. `pycowsay`).
///
/// `VersionInfo::assets` stays empty — wheel selection needs the full lock
/// plus a per-(platform, variant) target, which only the plan phase builds.
///
/// # Errors
///
/// Returns an error when the lock cannot be read/parsed, or when no locked
/// package matches `app_name`.
pub async fn list_versions(spec_dir: &Path, path: &str, app_name: &str) -> anyhow::Result<Vec<VersionInfo>> {
    let lock = load(spec_dir, path).await?;
    let version = app_version(&lock, app_name)?;
    let is_prerelease = Version::parse(&version).is_some_and(|v| v.prerelease().is_some());

    Ok(vec![VersionInfo {
        version,
        assets: HashMap::new(),
        is_prerelease,
    }])
}

/// Finds the locked package matching `app_name` (PEP 503 normalized) and
/// returns its pinned version.
///
/// # Errors
///
/// Returns an error when no locked package normalizes to `app_name`.
pub fn app_version(lock: &Pylock, app_name: &str) -> anyhow::Result<String> {
    Ok(find_app_package(lock, app_name)?.version.clone())
}

/// Finds the locked package matching `app_name` (PEP 503 normalized).
///
/// Shared by [`app_version`] and the describe-phase catalog autogen (which
/// also needs the package's wheel list, not just its version).
///
/// # Errors
///
/// Returns an error when no locked package normalizes to `app_name`.
pub fn find_app_package<'a>(lock: &'a Pylock, app_name: &str) -> anyhow::Result<&'a LockedPackage> {
    let normalized_app_name = normalize_package_name(app_name);
    lock.packages
        .iter()
        .find(|package| normalize_package_name(&package.name) == normalized_app_name)
        .ok_or_else(|| {
            let locked: Vec<&str> = lock.packages.iter().map(|p| p.name.as_str()).collect();
            anyhow::anyhow!("app package '{app_name}' not found in pylock.toml (locked packages: {locked:?})")
        })
}

/// PEP 503 normalization: lowercase, runs of `-`/`_`/`.` collapsed to a
/// single `-`. Mirrors `ocx_python::naming`'s private normalizer (not part of
/// that crate's public API) so mirror-side app-name matching honors the same
/// convention without widening the crate's surface for this one caller.
fn normalize_package_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut last_was_separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !last_was_separator {
                normalized.push('-');
                last_was_separator = true;
            }
        } else {
            normalized.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-py3-none-any.whl"
url = "https://example.com/pycowsay-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }

[[packages]]
name = "six"
version = "1.16.0"

[[packages.wheels]]
name = "six-1.16.0-py3-none-any.whl"
url = "https://example.com/six-1.16.0-py3-none-any.whl"
hashes = { sha256 = "bbbb" }
"#;

    #[test]
    fn app_version_matches_exact_name() {
        let lock = ocx_python::parse_pylock(LOCK).unwrap();
        assert_eq!(app_version(&lock, "pycowsay").unwrap(), "1.0.0");
    }

    #[test]
    fn app_version_matches_pep_503_normalized_name() {
        let lock = ocx_python::parse_pylock(LOCK).unwrap();
        // "PyCowSay" normalizes to "pycowsay", matching the locked entry.
        assert_eq!(app_version(&lock, "PyCowSay").unwrap(), "1.0.0");
    }

    #[test]
    fn app_version_rejects_missing_package() {
        let lock = ocx_python::parse_pylock(LOCK).unwrap();
        let err = app_version(&lock, "not-in-lock").unwrap_err();
        assert!(err.to_string().contains("not-in-lock"));
    }

    #[tokio::test]
    async fn list_versions_reads_and_parses_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("pylock.toml"), LOCK).await.unwrap();

        let versions = list_versions(dir.path(), "pylock.toml", "pycowsay").await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, "1.0.0");
        assert!(!versions[0].is_prerelease);
        assert!(versions[0].assets.is_empty());
    }

    #[tokio::test]
    async fn list_versions_rejects_unknown_app_name() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("pylock.toml"), LOCK).await.unwrap();

        let err = list_versions(dir.path(), "pylock.toml", "acme-app").await.unwrap_err();
        assert!(err.to_string().contains("acme-app"));
    }

    #[tokio::test]
    async fn list_versions_surfaces_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = list_versions(dir.path(), "missing.toml", "pycowsay").await.unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }

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
    }

    #[tokio::test]
    async fn classify_error_maps_missing_file_to_source_error() {
        // A genuinely unreadable file is an unavailable source, not bad data.
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path(), "missing.toml").await.unwrap_err();
        let mirror_err = classify_error("failed to load pylock source", err);
        assert!(matches!(mirror_err, MirrorError::SourceError(_)), "got: {mirror_err:?}");
    }

    #[test]
    fn normalize_package_name_matches_pep_503() {
        assert_eq!(normalize_package_name("Flask_Cors"), "flask-cors");
        assert_eq!(normalize_package_name("PyCowSay"), "pycowsay");
        assert_eq!(normalize_package_name("A.B_C-D"), "a-b-c-d");
    }
}
