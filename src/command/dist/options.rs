// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Shared flags for the `ocx-mirror dist` verbs.

use crate::command::package::options::OutputFormat;

/// Flags for `dist sync`.
///
/// Deliberately narrower than `RegistrySyncOptions`. There is no `--cache-dir`
/// because the run keeps no delta state — the destination is asked with a HEAD
/// instead — and no `--fail-fast` because a failed archive already stops the
/// run before anything is published.
#[derive(Clone, clap::Args)]
pub struct DistSyncOptions {
    /// Resolve and filter the manifest, and report what would be mirrored,
    /// without downloading, writing, or uploading anything
    #[arg(long)]
    pub dry_run: bool,

    /// Output format
    #[arg(long, value_enum, default_value = "plain")]
    pub format: OutputFormat,
}
