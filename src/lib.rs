// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror` — mirror upstream tool releases into OCI registries as OCX
//! packages.
//!
//! The binary is a thin `main` over this library. The library exists so the
//! test corpus can split: unit tests stay beside the code they exercise, and
//! the tests that only drive a command end to end move to `tests/`.
//!
//! # Public surface
//!
//! Deliberately small. Everything not listed here is `pub(crate)` or private,
//! which keeps the `dead_code` lint useful — a `pub` item in a library is
//! never dead, so a wide surface would silence a warning this crate relies on
//! (`[lints.rust] warnings = "deny"`).
//!
//! - [`Command`] — the CLI dispatcher `main` calls.
//! - [`error`] — [`MirrorError`](error::MirrorError) and its exit-code mapping.
//! - [`spec`] — the mirror spec: parsing, validation, and the types it yields,
//!   driven by `tests/spec_validation.rs`.

mod annotations;
mod auth;
mod command;
mod discord;
pub mod error;
mod filter;
mod http;
mod junit;
mod normalizer;
mod pipeline;
mod resolver;
mod run_summary;
mod source;
pub mod spec;
#[cfg(test)]
mod test_support;
mod version_platform_map;

pub use command::Command;
