// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The ocx-mirror pipeline: everything between a parsed spec and a published
//! artifact — the package orchestrator (download, verify, bundle), the `ocx`
//! push / sign / announce subprocesses, the Python env leg, `registry sync`,
//! `dist sync` and the registry-to-registry copy beneath them.
//!
//! The `ocx_cli` module is the mirror's own wrapper around the `ocx` binary;
//! it is always reached as `crate::ocx_cli::…` or `super::…`, never with
//! `ocx_cli::` at a path root, which ocx's satellite scan reads as the
//! forbidden `ocx_cli` crate (adr_bazel_crate_split.md § C1).

// `dist_sync` mirrors the *bootstrap* layer — ocx's own release archives and
// `dist.json` — rather than OCX packages, so it shares nothing with
// `registry_sync` beyond the download and verify helpers below.
pub mod dist_sync;
pub mod download;
pub mod lock_derive;
pub mod mirror_result;
pub mod mirror_task;
pub mod ocx_cli;
pub mod options;
pub mod orchestrator;
pub mod package;
pub mod progress;
pub mod push;
pub mod python_prepare;
pub mod python_push;
// `registry_copy` is a sibling of `registry_sync`, not a child: it is shared
// machinery below the command layer, and flat siblings with per-module child
// directories is this module's own shape.
pub mod registry_copy;
pub mod registry_sync;
pub mod sign_backfill;
pub mod target_registry;
pub mod verify;
