// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Deterministic wheel → `tar.zst` repack with `.data` relocation.
//!
//! Reads a wheel zip and writes a single deterministic `tar.zst` layer
//! (sorted entries, epoch mtimes, uid/gid 0, normalized modes, pinned zstd
//! level — the [`REPACK_VERSION`] convention). The written layer holds the
//! **final relocated tree** for the wheel: `purelib`/`platlib` →
//! `lib/site-packages/`, `.data/scripts` → `bin/`, `.data/data` → the content
//! root (`share/…`). Because one wheel spans three destination prefixes — which
//! a single layer prefix cannot express — the layer applies at the content root
//! with an empty [`LayerLayoutSpec`](ocx_lib::oci::LayerLayoutSpec); the tar
//! already carries the final paths.
//!
//! Extracts the RAW `[console_scripts]` object references from entry-point
//! metadata (the `module[:attr…]` grammar is parsed later, in `compose`, next
//! to shim synthesis) and the `RECORD` for the collision pre-check.

use std::path::{Path, PathBuf};

/// The repack-determinism grammar version, stamped as a `repack-vN` annotation.
///
/// Single source of truth for the deterministic-repack convention (sorted
/// entries, epoch mtimes, uid/gid 0, normalized modes, pinned zstd level).
/// Parallels [`L2_GRAMMAR_VERSION`](crate::platform::L2_GRAMMAR_VERSION).
pub const REPACK_VERSION: &str = "repack-v1";

/// A repacked wheel layer plus the metadata `compose` and `collide` need.
#[derive(Debug, Clone)]
pub struct RepackedWheel {
    /// The source wheel filename (e.g.
    /// `numpy-2.1.3-cp313-cp313-manylinux_2_28_x86_64.whl`).
    ///
    /// Carried through so `collide` can cite a human-readable wheel in
    /// [`CollisionError`](crate::collide::CollisionError) (not an opaque
    /// sha256) and `compose` can parse the ABI tag from it for the
    /// interpreter-consistency check.
    pub filename: String,
    /// Path to the written `tar.zst` layer.
    pub layer_path: PathBuf,
    /// The OCI digest of the layer (`sha256:…`).
    pub layer_digest: String,
    /// The `sha256` of the source wheel (for content-addressed naming).
    pub wheel_sha256: String,
    /// The `[console_scripts]` entry points (raw object references), for
    /// entrypoint synthesis in `compose`.
    pub entry_points: Vec<ConsoleScript>,
    /// Every installed path from the wheel `RECORD` (post-relocation), for the
    /// cross-wheel collision pre-check.
    pub record_paths: Vec<String>,
    /// The extras this wheel's scripts are gated on (union across its
    /// `[console_scripts]`), so `compose` can honor extras gating.
    pub locked_extras: Vec<String>,
}

/// A `[console_scripts]` entry point, as extracted from the wheel.
///
/// `repack` extracts the RAW object reference verbatim; the
/// `module[:attr[.attr…]]` grammar is parsed by `compose` when it synthesizes
/// the `importlib.import_module` + `getattr`-walk shim (co-locating the
/// entry-point grammar one-way-door with shim synthesis, where a malformed
/// reference surfaces as [`ComposeError::InvalidEntryPoint`](crate::compose::ComposeError)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleScript {
    /// The script name (the generated launcher's invocable name).
    pub name: String,
    /// The raw object reference `module[:attr[.attr…]]` (e.g. `"black:patched_main"`
    /// or module-only `"flask.cli"`), unparsed — `compose` parses it.
    pub reference: String,
    /// The extras that must be requested for this script to be synthesized
    /// (empty = always synthesized — e.g. `blackd = blackd:main [d]` gates on `d`).
    pub extras: Vec<String>,
}

/// Repacks a wheel into a deterministic `tar.zst` layer under `output_dir`.
///
/// The signature is `async` as the contract; the CPU-bound zip read + tar/zstd
/// write may run on `spawn_blocking` in the implementation.
///
/// # Errors
///
/// Returns [`RepackError::Io`] on a filesystem failure and
/// [`RepackError::Zip`] when the wheel is not a readable zip.
pub async fn repack_wheel(wheel_path: &Path, output_dir: &Path) -> Result<RepackedWheel, RepackError> {
    let _ = (wheel_path, output_dir);
    unimplemented!("W1.6")
}

/// Errors from repacking a wheel.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RepackError {
    /// A filesystem read/write failed.
    #[error("I/O error repacking wheel")]
    Io(#[source] std::io::Error),
    /// The wheel could not be read as a zip archive.
    #[error("failed to read wheel zip")]
    Zip(#[source] zip::result::ZipError),
}
