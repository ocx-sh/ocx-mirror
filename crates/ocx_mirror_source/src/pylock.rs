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
use ocx_package::version::Version;
use ocx_python::{LockedPackage, Pylock};

use super::VersionInfo;

/// Reads and parses the `pylock.toml` at `path` (resolved relative to
/// `spec_dir`).
///
/// # Errors
///
/// Returns an error when the file cannot be read or fails PEP 751 parsing.
/// Uses `.context()`/`.with_context()` rather than `anyhow::anyhow!(...)` so
/// the original `std::io::Error` / [`LockError`](ocx_python::LockError) stays
/// in the chain — `ocx_mirror_error::pylock::classify_error` downcasts it to
/// tell a bad file from bad content.
pub async fn load(spec_dir: &Path, path: &str) -> anyhow::Result<Pylock> {
    let lock_path = spec_dir.join(path);
    let contents = tokio::fs::read_to_string(&lock_path)
        .await
        .with_context(|| format!("failed to read pylock file '{}'", lock_path.display()))?;
    // Named type: the 65/69 split downcasts to `LockError` at runtime, so an
    // upstream error-type change must break the build, not turn 65 into 1.
    let parsed: Result<Pylock, ocx_python::LockError> = ocx_python::parse_pylock(&contents);
    parsed.context("failed to parse pylock.toml")
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
        asset_digests: HashMap::new(),
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
    lock.find_package(app_name).ok_or_else(|| {
        let locked: Vec<&str> = lock.packages.iter().map(|p| p.name.as_str()).collect();
        anyhow::anyhow!("app package '{app_name}' not found in pylock.toml (locked packages: {locked:?})")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocx_python::normalize_package_name;

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

    #[test]
    fn app_version_not_found_message_is_byte_exact() {
        // Characterization (plan_bazel_phase2 gate 1): these bytes reach
        // operator stderr and must survive the move onto `Pylock::find_package`.
        // Locked names are listed as written, not normalised.
        let lock = ocx_python::parse_pylock(
            r#"
lock-version = "1.0"

[[packages]]
name = "Alpha_Pkg"
version = "1.0.0"

[[packages.wheels]]
name = "alpha_pkg-1.0.0-py3-none-any.whl"
url = "https://example.com/alpha_pkg-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }

[[packages]]
name = "beta"
version = "2.0.0"

[[packages.wheels]]
name = "beta-2.0.0-py3-none-any.whl"
url = "https://example.com/beta-2.0.0-py3-none-any.whl"
hashes = { sha256 = "bbbb" }
"#,
        )
        .unwrap();
        let err = app_version(&lock, "missing-app").unwrap_err();
        assert_eq!(
            err.to_string(),
            "app package 'missing-app' not found in pylock.toml (locked packages: [\"Alpha_Pkg\", \"beta\"])"
        );
    }

    #[test]
    fn app_version_returns_first_match_in_lock_order() {
        // Two entries normalising to the same name: the first in lock order wins.
        let lock = ocx_python::parse_pylock(
            r#"
lock-version = "1.0"

[[packages]]
name = "app"
version = "1.0.0"

[[packages.wheels]]
name = "app-1.0.0-py3-none-any.whl"
url = "https://example.com/app-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }

[[packages]]
name = "App"
version = "2.0.0"

[[packages.wheels]]
name = "app-2.0.0-py3-none-any.whl"
url = "https://example.com/app-2.0.0-py3-none-any.whl"
hashes = { sha256 = "bbbb" }
"#,
        )
        .unwrap();
        assert_eq!(app_version(&lock, "APP").unwrap(), "1.0.0");
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

    #[test]
    fn normalize_package_name_matches_pep_503() {
        assert_eq!(normalize_package_name("Flask_Cors"), "flask-cors");
        assert_eq!(normalize_package_name("PyCowSay"), "pycowsay");
        assert_eq!(normalize_package_name("A.B_C-D"), "a-b-c-d");
    }
}
