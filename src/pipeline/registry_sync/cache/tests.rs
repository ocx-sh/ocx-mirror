// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Unit tests for the cache-root/digest-file/locks-dir path derivation
//! (C-037, C-038) and the digest read/record round trip (C-039).

use std::sync::Mutex;

use tempfile::TempDir;

use super::*;

// ── cache_root / default_cache_root ─────────────────────────────────────────

#[test]
fn cache_root_returns_the_cli_override_verbatim() {
    let override_path = Path::new("/custom/cache");
    assert_eq!(cache_root(Some(override_path)), override_path);
}

#[test]
fn default_cache_root_prefers_xdg_cache_home() {
    let root = default_cache_root(
        Some(OsString::from("/xdg-cache")),
        Some(PathBuf::from("/home/operator")),
    );
    assert_eq!(root, PathBuf::from("/xdg-cache/ocx-mirror"));
}

#[test]
fn default_cache_root_treats_empty_xdg_cache_home_as_unset() {
    // XDG Base Directory spec: an empty value is equivalent to unset.
    let root = default_cache_root(Some(OsString::new()), Some(PathBuf::from("/home/operator")));
    assert_eq!(root, PathBuf::from("/home/operator/.cache/ocx-mirror"));
}

#[test]
fn default_cache_root_treats_a_relative_xdg_cache_home_as_unset() {
    // XDG Base Directory spec: paths that are not absolute are ignored --
    // otherwise the cache location would silently depend on the invoking CWD.
    let root = default_cache_root(
        Some(OsString::from("relative/cache")),
        Some(PathBuf::from("/home/operator")),
    );
    assert_eq!(root, PathBuf::from("/home/operator/.cache/ocx-mirror"));
}

#[test]
fn default_cache_root_falls_back_to_home_dot_cache() {
    let root = default_cache_root(None, Some(PathBuf::from("/home/operator")));
    assert_eq!(root, PathBuf::from("/home/operator/.cache/ocx-mirror"));
}

#[test]
fn default_cache_root_falls_back_to_a_relative_dot_cache_with_no_home() {
    let root = default_cache_root(None, None);
    assert_eq!(root, PathBuf::from(".cache/ocx-mirror"));
}

// ── digest_file / locks_dir shape (C-038) ────────────────────────────────────

#[tokio::test]
async fn digest_file_and_locks_dir_match_the_c038_shape() {
    let cache = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    let hash = output_digest(output.path()).await.unwrap();

    assert_eq!(hash.len(), 64, "sha256 hex digest must be 64 characters");
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

    assert_eq!(
        digest_file(cache.path(), output.path(), "ocx.sh").await.unwrap(),
        cache.path().join("registry-sync").join(&hash).join("ocx.sh.digest"),
    );
    assert_eq!(
        locks_dir(cache.path(), output.path()).await.unwrap(),
        cache.path().join("registry-sync").join("locks").join(&hash),
    );
}

#[tokio::test]
async fn digest_file_is_deterministic_and_scoped_by_as_name() {
    let cache = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();

    let first = digest_file(cache.path(), output.path(), "ocx.sh").await.unwrap();
    let second = digest_file(cache.path(), output.path(), "ocx.sh").await.unwrap();
    assert_eq!(first, second, "path derivation must be a pure function of its inputs");

    let other_source = digest_file(cache.path(), output.path(), "example.com").await.unwrap();
    assert_eq!(
        first.parent(),
        other_source.parent(),
        "two sources sharing one output tree share one hash directory"
    );
    assert_ne!(first, other_source);
}

#[tokio::test]
async fn digest_file_and_locks_dir_error_when_output_does_not_exist() {
    let cache = TempDir::new().unwrap();
    let missing_output = cache.path().join("does-not-exist");

    assert!(matches!(
        digest_file(cache.path(), &missing_output, "ocx.sh").await,
        Err(MirrorError::IndexWriteError(_))
    ));
    assert!(matches!(
        locks_dir(cache.path(), &missing_output).await,
        Err(MirrorError::IndexWriteError(_))
    ));
}

// ── Containment: the load-bearing property (C-038) ───────────────────────────

/// Serialises tests in this file that mutate the process-wide current
/// directory, and restores it on drop (including on panic/assertion failure)
/// so a red assertion never leaves every later test in the binary running
/// from the wrong directory. Scoped to this file rather than reusing the
/// crate-wide `OCX_ENV_LOCK`: that lock documents itself as covering the
/// `OCX_*` environment specifically, and CWD is a different piece of global
/// state that no other test in this crate touches today.
static CWD_LOCK: Mutex<()> = Mutex::new(());

struct RestoreCwd {
    original: PathBuf,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl RestoreCwd {
    fn enter(dir: &Path) -> Self {
        let guard = CWD_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Self {
            original,
            _guard: guard,
        }
    }
}

impl Drop for RestoreCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Asserts both `locks_dir` and `digest_file` land under `cache_root` and
/// never under `output`, for one spelling of `output`.
///
/// Both sides of the containment check are canonicalised before comparing:
/// `derived` is rebuilt from `cache_root`'s own canonical form rather than
/// compared as-is, because a raw tempdir path is not necessarily canonical
/// (macOS resolves `/tmp` through a symlink) — and containment must be
/// checked by `Path::starts_with` component matching, never a string prefix
/// (`/out-of-tree` starts with the string `/out` without being inside it).
async fn assert_contained_outside_output(
    cache_root: &Path,
    canonical_cache: &Path,
    output_shape: &Path,
    canonical_output: &Path,
) {
    let locks = locks_dir(cache_root, output_shape).await.unwrap();
    let digest = digest_file(cache_root, output_shape, "ocx.sh").await.unwrap();

    for derived in [locks, digest] {
        assert!(
            derived.starts_with(cache_root),
            "{derived:?} must be built under the cache root {cache_root:?}"
        );
        let canonical_derived = canonical_cache.join(derived.strip_prefix(cache_root).unwrap());
        assert!(
            !canonical_derived.starts_with(canonical_output),
            "{canonical_derived:?} must not be nested under the served output tree {canonical_output:?}",
        );
    }
}

/// Stays a plain `#[test]` driving its own runtime rather than a
/// `#[tokio::test]`: the relative-path shape below needs `CWD_LOCK` held while
/// the (now async) derivation runs, and holding a `std::sync::MutexGuard`
/// across an `.await` is the Block-tier pattern `quality-rust.md` forbids.
/// `block_on` keeps the guard inside one blocking call instead.
#[test]
fn digest_file_and_locks_dir_stay_outside_output_for_every_output_shape() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cache = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    std::fs::create_dir(output.path().join("nested")).unwrap();

    let canonical_cache = std::fs::canonicalize(cache.path()).unwrap();
    let canonical_output = std::fs::canonicalize(output.path()).unwrap();

    // Trailing separator.
    let mut trailing_separator = output.path().as_os_str().to_os_string();
    trailing_separator.push(std::path::MAIN_SEPARATOR_STR);
    let trailing_separator = PathBuf::from(trailing_separator);

    // `..` in the middle, resolving back to the same directory.
    let dot_dot = output.path().join("nested").join("..");

    for shape in [output.path().to_path_buf(), trailing_separator, dot_dot] {
        runtime.block_on(assert_contained_outside_output(
            cache.path(),
            &canonical_cache,
            &shape,
            &canonical_output,
        ));
    }

    // A genuinely relative path can only be produced by changing the
    // process's current directory.
    let relative_output = PathBuf::from(output.path().file_name().unwrap());
    let _cwd = RestoreCwd::enter(output.path().parent().unwrap());
    runtime.block_on(assert_contained_outside_output(
        cache.path(),
        &canonical_cache,
        &relative_output,
        &canonical_output,
    ));
}

// ── read_recorded_digest / record_digest (C-039) ─────────────────────────────

#[tokio::test]
async fn read_recorded_digest_is_none_when_absent() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ocx.sh.digest");
    assert_eq!(read_recorded_digest(&path).await.unwrap(), None);
}

#[tokio::test]
async fn read_recorded_digest_trims_whitespace() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ocx.sh.digest");
    tokio::fs::write(&path, b"  abc123  \n").await.unwrap();
    assert_eq!(read_recorded_digest(&path).await.unwrap(), Some("abc123".to_string()));
}

#[tokio::test]
async fn read_recorded_digest_is_none_for_an_empty_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ocx.sh.digest");
    tokio::fs::write(&path, b"").await.unwrap();
    assert_eq!(read_recorded_digest(&path).await.unwrap(), None);
}

#[tokio::test]
async fn read_recorded_digest_is_none_for_non_utf8_content() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ocx.sh.digest");
    tokio::fs::write(&path, [0xff_u8, 0xfe, 0xfd]).await.unwrap();
    assert_eq!(read_recorded_digest(&path).await.unwrap(), None);
}

#[tokio::test]
async fn read_recorded_digest_errors_on_a_non_absent_io_failure() {
    let dir = TempDir::new().unwrap();
    // A directory where a file is expected fails to open for reasons other
    // than "absent" -- must surface, not silently read as "no digest yet".
    let path = dir.path().join("looks-like-a-digest-file");
    tokio::fs::create_dir(&path).await.unwrap();
    assert!(matches!(
        read_recorded_digest(&path).await,
        Err(MirrorError::IndexWriteError(_))
    ));
}

#[tokio::test]
async fn record_digest_creates_missing_parent_directories() {
    let cache = TempDir::new().unwrap();
    let path = cache
        .path()
        .join("registry-sync")
        .join("deadbeef")
        .join("ocx.sh.digest");

    record_digest(&path, "abc123").await.unwrap();

    assert_eq!(read_recorded_digest(&path).await.unwrap(), Some("abc123".to_string()));
}

#[tokio::test]
async fn record_digest_overwrites_a_previous_value() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ocx.sh.digest");

    record_digest(&path, "first").await.unwrap();
    record_digest(&path, "second").await.unwrap();

    assert_eq!(read_recorded_digest(&path).await.unwrap(), Some("second".to_string()));
}
