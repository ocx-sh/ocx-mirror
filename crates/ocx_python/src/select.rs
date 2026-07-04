// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Wheel selection: marker evaluation + tag-compatibility ranking per target.
//!
//! For a single `(variant, platform key)` [`PythonTarget`], selects exactly one
//! wheel per applicable package (design spec, "Wheel selection algorithm"):
//! filter packages by their PEP 508 marker against the derived marker
//! environment, then rank each package's candidate wheels by tag priority from
//! `uv-platform-tags`, tiebreaking by build tag then filename. Zero candidates
//! for an applicable package is an actionable [`SelectError::NoCompatibleWheel`].

use crate::lock::Pylock;
use crate::platform::PythonTarget;

/// A resolved wheel chosen for a package under a target.
#[derive(Debug, Clone)]
pub struct WheelRef {
    /// The distribution name (e.g. `"numpy"`).
    pub name: String,
    /// The pinned version (e.g. `"2.1.3"`).
    pub version: String,
    /// The wheel filename.
    pub filename: String,
    /// The wheel download URL, when the lock provides one.
    pub url: Option<String>,
    /// The wheel `sha256` hash (hex, no prefix).
    pub sha256: String,
}

/// Selects one wheel per applicable package for `target`.
///
/// Applicability is decided by each package's PEP 508 marker evaluated against
/// the target's derived marker environment; non-applicable packages (OS forks,
/// implementation forks) are dropped, not failed.
///
/// # Errors
///
/// Returns [`SelectError::NoCompatibleWheel`] when an applicable package has no
/// wheel intersecting the target tag set (naming the package, triple, variant,
/// and the tags that WERE available), and [`SelectError::AbiMismatch`] when a
/// selected binary wheel's ABI is inconsistent with the interpreter pin.
/// Tag-parse failures surface as [`SelectError::Platform`].
pub fn select_wheels(lock: &Pylock, target: &PythonTarget) -> Result<Vec<WheelRef>, SelectError> {
    let _ = (lock, target);
    unimplemented!("W1.3")
}

/// Errors from wheel selection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelectError {
    /// No wheel for an applicable package intersects the target tag set.
    ///
    /// Names the package, the target triple, the variant, and the tags that
    /// were available on the package's wheels — distinguishing
    /// no-wheel-for-triple (e.g. `psycopg2`) from no-wheel-anywhere
    /// (e.g. `uwsgi`).
    #[error(
        "no compatible wheel for package '{package}' on target '{target}' (variant '{variant}'); available tags: {available_tags:?}"
    )]
    NoCompatibleWheel {
        /// The package with no compatible wheel.
        package: String,
        /// The target triple (os/arch/libc).
        target: String,
        /// The variant name/constraints.
        variant: String,
        /// The platform tags present on the package's candidate wheels.
        available_tags: Vec<String>,
    },
    /// A selected binary wheel's ABI is inconsistent with the interpreter pin
    /// (e.g. `cp313` wheel against a `cp313t` free-threaded interpreter).
    #[error("wheel '{filename}' ABI '{wheel_abi}' is incompatible with interpreter ABI '{interpreter_abi}'")]
    AbiMismatch {
        /// The offending wheel filename.
        filename: String,
        /// The wheel's ABI tag.
        wheel_abi: String,
        /// The interpreter's ABI tag.
        interpreter_abi: String,
    },
    /// A wheel filename or platform tag failed to parse during ranking.
    #[error("platform tag error during selection")]
    Platform(#[from] crate::platform::PlatformError),
}
