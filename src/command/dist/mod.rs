// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror dist` subcommand group.
//!
//! Third sibling to [`super::package`] and [`super::registry`], and the only
//! one that touches no OCI registry at all. `package` mirrors one upstream
//! tool's releases as OCX packages; `registry` copies whole index sources into
//! a corporate registry; `dist` mirrors the **bootstrap layer** — ocx's own
//! release archives and the `dist.json` manifest every installer, Bazel rule,
//! CMake module and SDK resolves versions and checksums from — into a store a
//! restricted network can reach.

// Private, unlike `command::registry`'s: `pipeline::dist_sync` takes a bare
// `dry_run: bool` rather than this struct, so nothing below the command layer
// needs to name it.
mod options;
mod sync;

use ocx_lib::cli::DataInterface;

use crate::error::MirrorError;

/// Dispatcher for `ocx-mirror dist <subcommand>`.
#[derive(clap::Subcommand)]
pub enum DistCommand {
    /// Mirror the OCX distribution into a generic store
    Sync(sync::Sync),
}

impl DistCommand {
    pub async fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        match self {
            Self::Sync(cmd) => cmd.execute(printer).await,
        }
    }
}
