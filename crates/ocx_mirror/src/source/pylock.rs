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

use ocx_lib::package::version::Version;
use ocx_python::Pylock;

use super::VersionInfo;

/// Reads and parses the `pylock.toml` at `path` (resolved relative to
/// `spec_dir`).
///
/// # Errors
///
/// Returns an error when the file cannot be read or fails PEP 751 parsing.
pub async fn load(spec_dir: &Path, path: &str) -> anyhow::Result<Pylock> {
    let lock_path = spec_dir.join(path);
    let contents = tokio::fs::read_to_string(&lock_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read pylock file '{}': {e}", lock_path.display()))?;
    ocx_python::parse_pylock(&contents).map_err(|e| anyhow::anyhow!("failed to parse pylock.toml: {e}"))
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
    let normalized_app_name = normalize_package_name(app_name);
    lock.packages
        .iter()
        .find(|package| normalize_package_name(&package.name) == normalized_app_name)
        .map(|package| package.version.clone())
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

    #[test]
    fn normalize_package_name_matches_pep_503() {
        assert_eq!(normalize_package_name("Flask_Cors"), "flask-cors");
        assert_eq!(normalize_package_name("PyCowSay"), "pycowsay");
        assert_eq!(normalize_package_name("A.B_C-D"), "a-b-c-d");
    }
}
