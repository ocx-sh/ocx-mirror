// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Publish-time collision pre-check across a selected wheel set.
//!
//! A valid resolved lock is collision-free by construction, so OCX's
//! overlap-free prefix-layer union composes a correct `site-packages`. This
//! check is the guard that proves the invariant holds for a concrete wheel set
//! *before* anything is pushed: if two repacked wheels claim the same installed
//! path (post-relocation), the union would be ambiguous — a hard
//! [`CollisionError`], failing before push rather than corrupting the registry.

use crate::repack::RepackedWheel;

/// Verifies that no two wheels in the set share an installed path.
///
/// Compares the `record_paths` of every [`RepackedWheel`]; PEP 420 namespace
/// package directories are shared by design and are not collisions.
///
/// # Errors
///
/// Returns [`CollisionError::OverlappingPaths`] naming the conflicting path and
/// the two wheels that both claim it.
pub fn check_collisions(wheels: &[RepackedWheel]) -> Result<(), CollisionError> {
    let _ = wheels;
    unimplemented!("W1.5")
}

/// Errors from the collision pre-check.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CollisionError {
    /// Two wheels claim the same installed path.
    #[error("path '{path}' is claimed by both wheel '{first_wheel}' and wheel '{second_wheel}'")]
    OverlappingPaths {
        /// The colliding installed path.
        path: String,
        /// The first wheel claiming the path.
        first_wheel: String,
        /// The second wheel claiming the path.
        second_wheel: String,
    },
}
