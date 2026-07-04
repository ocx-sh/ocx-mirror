// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Pure translation library: PEP 751 `pylock.toml` in → OCX package
//! compositions out.
//!
//! `ocx_python` encodes the cross-repo Python-on-OCX conventions (wheel
//! naming, repack determinism, layer layout, entrypoint synthesis,
//! platform/axis encoding) exactly once, so that every writer of the shared
//! registry namespace — `ocx-mirror` today, `ocx-dist` later — produces
//! byte-compatible artifacts. See `.claude/artifacts/design_spec_ocx_python.md`.
//!
//! # Boundary
//!
//! This crate performs **no registry I/O**: no `Publisher`, no HTTP download,
//! no registry existence checks. Filesystem I/O (reading wheels, writing
//! repacked layers) is in scope; everything network-facing is the consumer's
//! responsibility (e.g. the mirror's `pipeline/download.rs`). It also stays
//! **target-agnostic**: it emits repo-relative identifiers and OCX metadata
//! but never knows a concrete registry host — the consumer supplies the
//! registry and assembles the final [`ocx_lib::oci::Identifier`].
//!
//! # Pipeline
//!
//! ```text
//! parse_pylock ─▶ select_wheels ─▶ repack_wheel ─▶ check_collisions ─▶ compose_env
//!    (lock)         (select)          (repack)         (collide)          (compose)
//! ```
//!
//! `wheel_reference` (naming) renders the conventional repo path for each
//! selected wheel; `platform` holds the L1 wheel-tag→facts and L2 facts→OCX
//! encoding model that `select` and `compose` share.

pub mod collide;
pub mod compose;
pub mod error;
pub mod lock;
pub mod naming;
pub mod platform;
pub mod repack;
pub mod select;

// ── Public entry points (re-exported at the crate root for ergonomics) ──────

pub use collide::{CollisionError, check_collisions};
pub use compose::{ComposeError, EnvComposition, EnvSpec, WheelLayer, compose_env};
pub use lock::{LockError, LockedPackage, LockedWheel, Pylock, parse_pylock};
pub use naming::{WheelReference, WheelScope, wheel_reference};
pub use platform::{
    Implementation, InterpreterPin, L2_GRAMMAR_VERSION, LibcConstraint, LibcFamily, MarkerEnvironment,
    OcxPlatformEncoding, PlatformError, PlatformFacts, PythonTarget, TargetArchitecture, TargetOperatingSystem,
    TargetPlatform, VariantConstraints, encode_l2, marker_environment, parse_platform_tag,
};
pub use repack::{ConsoleScript, REPACK_VERSION, RepackError, RepackedWheel, repack_wheel};
pub use select::{SelectError, WheelRef, select_wheels};
