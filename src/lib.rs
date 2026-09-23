// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror` — mirror upstream tool releases into OCI registries as OCX
//! packages.
//!
//! The binary is a thin `main` over this library, and this library is the
//! top of a workspace: it holds the CLI glue (`command/` — argument parsing,
//! subcommand dispatch, the CI renderer) and nothing below it. The layers the
//! commands drive are workspace members under `crates/`, and which may depend
//! on which is `crates/crate_map.toml`, enforced by
//! `tests/workspace_structure.rs`:
//!
//! - `ocx_mirror_pipeline` — the mirror pipeline (download, package, push,
//!   sign), registry sync and dist sync.
//! - `ocx_mirror_spec` — the mirror spec and its validation.
//! - `ocx_mirror_error` — [`MirrorError`](error::MirrorError) and exit codes.
//! - `ocx_mirror_source` — upstream adapters (GitHub Releases, URL indexes,
//!   pylock/PyPI) and version resolution.
//! - `ocx_mirror_report` — JUnit, run summaries, Discord.
//! - `ocx_mirror_http` — HTTP client, TLS roots, retry, credentials.
//!
//! Debug-log targets follow the crate (`ocx_mirror_pipeline::…`), so the
//! `RUST_LOG=info,ocx_mirror=debug` recipe reaches every member by prefix —
//! pinned by `tests/log_targets.rs`.
//!
//! # Public surface
//!
//! Deliberately small, and unchanged by the split: every path public before
//! it still resolves here. Everything not listed is private to this crate,
//! which keeps the `dead_code` lint useful — a `pub` item in a library is
//! never dead, so a wide surface would silence a warning this crate relies on
//! (`[workspace.lints.rust] warnings = "deny"`).
//!
//! - [`Command`] — the CLI dispatcher `main` calls.
//! - [`error`] — [`MirrorError`](error::MirrorError) and its exit-code mapping
//!   (the `ocx_mirror_error` crate).
//! - [`spec`] — the mirror spec: parsing, validation, and the types it yields,
//!   driven by `tests/spec_validation.rs` (the `ocx_mirror_spec` crate).
//! - [`install_extra_roots`] — the `OCX_EXTRA_CA_CERTS` startup gate. Public
//!   because the binary runs it *before* [`Command`] dispatch and must
//!   classify the `TlsError` it raises into an exit code itself.

mod command;

// Private aliases (D-P1): the workspace crates bound under their pre-split
// module names, so the `crate::http::…` / `crate::pipeline::…` /
// `crate::junit::…` … paths in `command/` keep resolving. Never `pub` — the
// public surface is the four items below.
use ocx_mirror_http as http;
use ocx_mirror_pipeline as pipeline;
use ocx_mirror_report::{discord, junit, run_summary};
use ocx_mirror_source as source;
use ocx_mirror_source::resolver;
use ocx_mirror_spec::{annotations, filter, normalizer, version_platform_map};

// Test scaffolding lives in its own dev-only crate; the alias keeps every
// `crate::test_support::…` path in this crate's tests resolving.
#[cfg(test)]
use ocx_mirror_test_support as test_support;

pub use command::Command;
pub use ocx_mirror_error as error;
pub use ocx_mirror_http::install_extra_roots;
pub use ocx_mirror_spec as spec;
