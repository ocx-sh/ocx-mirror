// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror registry` subcommand group.
//!
//! Sibling to [`super::package`]. Where `package` mirrors one upstream tool's
//! releases, `registry` copies whole OCX index sources — every package a source
//! catalog lists — into a corporate registry, and writes a servable index tree
//! whose root documents point at where the bytes now are.
//!
//! See `.claude/artifacts/adr_registry_mirror_sync.md`.

// `RegistrySyncOptions` lives in `pipeline::options` (the pipeline takes the
// whole flag set); this module re-exports it at its old path.
pub(crate) mod options;
mod sync;

use ocx_console::DataInterface;

use crate::error::MirrorError;

/// Dispatcher for `ocx-mirror registry <subcommand>`.
#[derive(clap::Subcommand)]
pub enum RegistryCommand {
    /// Mirror whole index sources into a corporate registry
    Sync(sync::Sync),
}

impl RegistryCommand {
    pub async fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        match self {
            Self::Sync(cmd) => cmd.execute(printer).await,
        }
    }
}
