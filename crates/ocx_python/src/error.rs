// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Error taxonomy for `ocx_python`.
//!
//! Following the one-enum-per-module convention (see `quality-rust-errors.md`),
//! each stage owns its error type in its own module rather than a single
//! crate-wide god enum:
//!
//! | Error | Module | Raised by |
//! |---|---|---|
//! | [`LockError`](crate::lock::LockError) | `lock` | [`parse_pylock`](crate::parse_pylock) |
//! | [`PlatformError`](crate::platform::PlatformError) | `platform` | tag parse / L2 encode (internal source) |
//! | [`SelectError`](crate::select::SelectError) | `select` | [`select_wheels`](crate::select_wheels) |
//! | [`RepackError`](crate::repack::RepackError) | `repack` | [`repack_wheel`](crate::repack_wheel) |
//! | [`CollisionError`](crate::collide::CollisionError) | `collide` | [`check_collisions`](crate::check_collisions) |
//! | [`ComposeError`](crate::compose::ComposeError) | `compose` | [`compose_env`](crate::compose_env) |
//!
//! Every error type is `#[derive(thiserror::Error, Debug)]`, `#[non_exhaustive]`,
//! and carries wrapped sources via `#[source]`/`#[from]`. `PlatformError` is an
//! **internal** source type: it never surfaces to the consumer directly, only
//! wrapped inside [`SelectError`](crate::select::SelectError) or
//! [`ComposeError`](crate::compose::ComposeError).
//!
//! # Consumer mapping (`MirrorError`)
//!
//! The mirror wraps each public error in a `MirrorError` variant with a
//! `#[source]` chain and an exit-code mapping. This crate never imports
//! `MirrorError` or `ocx_lib::cli::ExitCode` — the mapping lives entirely on the
//! consumer side:
//!
//! | This crate | Exit | Rationale |
//! |---|---|---|
//! | [`LockError`](crate::lock::LockError) | 65 `DataError` | malformed lock, sdist-only, missing hash |
//! | [`SelectError`](crate::select::SelectError) | 65 `DataError` | no compatible wheel for the target |
//! | [`RepackError`](crate::repack::RepackError) | 1 `Failure` | I/O / zip read failure |
//! | [`CollisionError`](crate::collide::CollisionError) | 65 `DataError` | overlapping wheel paths (pre-push gate) |
//! | [`ComposeError`](crate::compose::ComposeError) | 65 `DataError` | ABI mismatch, bad entry point, unknown extra |
//!
//! This module intentionally declares no types of its own — it is the taxonomy
//! reference and re-export hub. The error types live with their stages.

pub use crate::collide::CollisionError;
pub use crate::compose::ComposeError;
pub use crate::lock::LockError;
pub use crate::platform::PlatformError;
pub use crate::repack::RepackError;
pub use crate::select::SelectError;
