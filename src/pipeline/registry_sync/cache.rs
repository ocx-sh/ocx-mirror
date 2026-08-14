// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The run's out-of-tree state: the source-catalog digest file and the lock
//! directory (C-037, C-038).
//!
//! Both live **outside** `output:`. The served tree is a distributable
//! artifact operators commit and `rsync`; a lock file or a cache file inside it
//! travels with every copy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::MirrorError;

/// Resolve the cache root: `--cache-dir` if given, else
/// `${XDG_CACHE_HOME:-~/.cache}/ocx-mirror/` (C-037).
pub fn cache_root(cli_override: Option<&Path>) -> PathBuf {
    match cli_override {
        Some(path) => path.to_path_buf(),
        None => default_cache_root(std::env::var_os("XDG_CACHE_HOME"), std::env::home_dir()),
    }
}

/// The `--cache-dir`-absent branch of [`cache_root`], with its two environment
/// reads taken as plain values.
///
/// The seam that lets a test drive the fallback chain without mutating
/// process-global environment state — which, on top of racing every other
/// test in the binary, has needed `unsafe` to write since Rust 1.82.
fn default_cache_root(xdg_cache_home: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    xdg_cache_home
        .map(PathBuf::from)
        // XDG Base Directory spec: an empty or non-absolute value counts as unset.
        .filter(|path| !path.as_os_str().is_empty() && path.is_absolute())
        .or_else(|| home.map(|home_dir| home_dir.join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("ocx-mirror")
}

/// `sha256(canonicalized output path)`, hex-encoded — the join key C-038's
/// digest file and locks dir share.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] when `output` cannot be canonicalized —
/// in practice, when it does not exist yet. Deriving these paths is the
/// caller's responsibility to sequence after the output tree exists.
fn output_digest(output: &Path) -> Result<String, MirrorError> {
    let canonical = std::fs::canonicalize(output).map_err(|e| {
        MirrorError::IndexWriteError(format!("cannot canonicalize output path '{}': {e}", output.display()))
    })?;
    // Hash the raw encoded bytes, not `Path::display()`'s lossy rendering --
    // two distinct non-UTF-8 paths must not collide onto the same digest.
    let hash = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    Ok(hex::encode(hash))
}

/// `<cache-root>/registry-sync/<sha256(canonicalized output path)>/<as>.digest`
/// (C-038).
///
/// Stable per output tree, never per run: it records the source-catalog digest
/// of the last **fully successful** run, so losing, staling or corrupting it
/// costs one extra comparison pass and never correctness. This is not a resume
/// journal.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] when `output` cannot be canonicalized.
pub fn digest_file(cache_root: &Path, output: &Path, as_name: &str) -> Result<PathBuf, MirrorError> {
    Ok(cache_root
        .join("registry-sync")
        .join(output_digest(output)?)
        .join(format!("{as_name}.digest")))
}

/// `<cache-root>/registry-sync/locks/<sha256(canonicalized output path)>/`
/// (C-038) — the `locks_root` every [`IndexStore`](ocx_lib::file_structure::IndexStore)
/// for this tree is redirected to.
///
/// **Stable per output tree, never per run.** A per-run lock directory would
/// give each run its own lock and defeat serialization entirely — two
/// concurrent runs would both enter a `CatalogTransaction` and one catalog
/// write would be lost. The stable directory makes a second concurrent run
/// block for up to `SOURCE_LOCK_TIMEOUT` and then error, which is the correct
/// outcome.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] when `output` cannot be canonicalized.
pub fn locks_dir(cache_root: &Path, output: &Path) -> Result<PathBuf, MirrorError> {
    Ok(cache_root
        .join("registry-sync")
        .join("locks")
        .join(output_digest(output)?))
}

/// Read the source-catalog digest the last fully successful run recorded, if
/// any (C-039).
///
/// An absent, unreadable or malformed file is `Ok(None)` — the short-circuit
/// simply does not fire and the run falls back to the per-root pass.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] only for an I/O failure that is not
/// "absent".
pub async fn read_recorded_digest(path: &Path) -> Result<Option<String>, MirrorError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(MirrorError::IndexWriteError(format!(
                "cannot read cached digest '{}': {e}",
                path.display()
            )));
        }
    };
    // Non-UTF-8 content is corruption, not an I/O failure -- same "just
    // compare again" outcome as an absent file, never a hard error.
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    let digest = text.trim();
    if digest.is_empty() {
        Ok(None)
    } else {
        Ok(Some(digest.to_string()))
    }
}

/// Record the source-catalog digest after a fully successful run (C-030).
///
/// Called **only** on full success: a run with any failure leaves the previous
/// value in place, so the next run re-compares instead of short-circuiting past
/// work that did not land.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] when the file cannot be written.
pub async fn record_digest(path: &Path, digest: &str) -> Result<(), MirrorError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            MirrorError::IndexWriteError(format!("cannot create cache directory '{}': {e}", parent.display()))
        })?;
    }
    // ponytail: plain write, not a tempfile+rename dance -- the doc above (and
    // C-039) already treats a lost or torn write as harmless (one extra
    // comparison pass next run), so an atomic publish would defend against a
    // risk the design says does not matter.
    tokio::fs::write(path, format!("{digest}\n"))
        .await
        .map_err(|e| MirrorError::IndexWriteError(format!("cannot write cached digest '{}': {e}", path.display())))
}

#[cfg(test)]
#[path = "cache/tests.rs"]
mod tests;
