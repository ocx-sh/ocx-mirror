// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

// `dist_sync` mirrors the *bootstrap* layer — ocx's own release archives and
// `dist.json` — rather than OCX packages, so it shares nothing with
// `registry_sync` beyond the download and verify helpers below.
pub mod dist_sync;
pub mod download;
pub(crate) mod lock_derive;
pub mod mirror_result;
pub mod mirror_task;
pub(crate) mod ocx_cli;
pub mod orchestrator;
pub mod package;
pub mod progress;
pub mod push;
pub(crate) mod python_prepare;
pub(crate) mod python_push;
// `registry_copy` is a sibling of `registry_sync`, not a child: it is shared
// machinery below the command layer, and flat siblings with per-module child
// directories is this module's own shape.
pub mod registry_copy;
pub mod registry_sync;
pub mod sign_backfill;
pub(crate) mod target_registry;
pub mod verify;
