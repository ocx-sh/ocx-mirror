// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Finding the bundles a `prepare` run left on disk, and mapping their file
//! names back onto spec platform keys.
//!
//! The slug in a bundle filename is the join key between `prepare`, the test
//! matrix and this command; a spec lookup is preferred to the textual
//! reversal, which only keeps the caller total for a key validation rejects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::MirrorError;
use crate::spec::{self, MirrorSpec};

/// Map `(version, platform_slug)` to the canonical bundle filename and path.
///
/// Bundles are named `bundle-{V}-{platform_slug}.tar.xz` in `bundles_dir`.
pub fn bundle_path_for(bundles_dir: &Path, version: &str, platform_slug: &str) -> PathBuf {
    bundles_dir.join(format!("bundle-{version}-{platform_slug}.tar.xz"))
}

/// Convert `linux/amd64` → `linux_amd64` (platform string → slug).
///
/// The canonical slug, shared with `pipeline prepare` (which names the work
/// directory) and the CI renderer (which names the JUnit file).
pub fn platform_to_slug(platform: &str) -> String {
    spec::platform_key_slug(platform)
}

/// Convert a bundle's platform slug back to its platform string
/// (`linux_amd64` → `linux/amd64`).
///
/// The spec's declared keys are consulted first, because the slug is lossy
/// wherever a platform carries `os.features`: `linux_amd64_libc.musl` has no
/// textual reversal to `linux/amd64+libc.musl`, and guessing `linux/amd64_libc.musl`
/// misses every subsequent `spec.platforms` lookup (container ids, test names)
/// and would hand `ocx package push` an unparseable `--platform`.
///
/// The `_`-splitting heuristic stays as the fallback for a bundle whose platform
/// the spec never declared under `platforms:`.
pub fn slug_to_platform(spec: &MirrorSpec, slug: &str) -> String {
    if let Some(platforms) = &spec.platforms
        && let Some(key) = platforms.keys().find(|key| platform_to_slug(key) == slug)
    {
        return key.clone();
    }
    slug_to_platform_heuristic(slug)
}

/// Best-effort textual reversal — replaces the first `_` that separates the OS
/// from the architecture. Known OS prefixes: `linux`, `darwin`, `windows`.
pub fn slug_to_platform_heuristic(slug: &str) -> String {
    for os in &["linux", "darwin", "windows"] {
        let prefix = format!("{os}_");
        if slug.starts_with(prefix.as_str()) {
            let arch = &slug[prefix.len()..];
            return format!("{os}/{arch}");
        }
    }
    // Fallback: replace first `_` with `/`.
    if let Some(pos) = slug.find('_') {
        let mut s = slug.to_string();
        s.replace_range(pos..pos + 1, "/");
        return s;
    }
    slug.to_string()
}

/// Enumerate bundles from `bundles_dir`, returning a map of
/// `version → {platform_slug set}`.
///
/// Bundle filenames follow `bundle-{V}-{platform_slug}.tar.xz`.
pub async fn enumerate_bundles(bundles_dir: &Path) -> Result<HashMap<String, Vec<String>>, MirrorError> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    let mut read_dir = tokio::fs::read_dir(bundles_dir).await.map_err(|e| {
        MirrorError::TemplateError(format!(
            "failed to read bundles directory {}: {e}",
            bundles_dir.display()
        ))
    })?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| MirrorError::TemplateError(format!("failed to iterate bundles directory: {e}")))?
    {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Parse `bundle-{V}-{platform_slug}.tar.xz`
        if let Some((version, platform_slug)) = parse_bundle_filename(&name) {
            map.entry(version.to_string())
                .or_default()
                .push(platform_slug.to_string());
        }
    }

    Ok(map)
}

/// Parse a bundle filename of the form `bundle-{V}-{platform_slug}.tar.xz`.
///
/// Returns `Some((version, platform_slug))` on success, `None` if the filename
/// does not match the expected pattern.
pub fn parse_bundle_filename(name: &str) -> Option<(&str, &str)> {
    let name = name.strip_prefix("bundle-")?;
    let name = name.strip_suffix(".tar.xz")?;

    // The remaining string is `{V}-{platform_slug}`. The platform slug contains
    // one `_` (e.g. `linux_amd64`). The version may contain `.` and digits.
    // Strategy: find the last `-` followed by a known platform slug prefix.
    // Known OS prefixes in slug form: `linux_`, `darwin_`, `windows_`.
    let platform_prefixes = ["linux_", "darwin_", "windows_"];
    for prefix in &platform_prefixes {
        // Find `-{prefix}` in the remaining string.
        let search = format!("-{prefix}");
        if let Some(pos) = name.rfind(search.as_str()) {
            let version = &name[..pos];
            let platform_slug = &name[pos + 1..];
            if !version.is_empty() && !platform_slug.is_empty() {
                return Some((version, platform_slug));
            }
        }
    }
    None
}
