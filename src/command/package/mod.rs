// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package` subcommand group.
//!
//! Mirrors upstream package releases into an OCI registry. Groups the one-shot
//! mirror verbs (`sync`, `check`, `validate`) with the pre-publish test
//! `pipeline`. Sibling namespace `registry` (reserved) will host
//! registry-to-registry mirroring; see `adr_cli_namespace_restructure`.

mod check;
mod options;
// `pub(crate)`: `pipeline::python_push` (outside this subtree) drives the env
// push through `pipeline::push::push_with_retry`, so both publish legs share
// one retry ladder and one transient-exit predicate.
pub(crate) mod pipeline;
mod sync;
// `pub(crate)`: `pipeline::python_push` (outside this subtree) reaches the
mod validate;

use ocx_lib::cli::DataInterface;
use ocx_lib::cli::progress::ProgressManager;

use crate::error::MirrorError;

/// Dispatcher for `ocx-mirror package <subcommand>`.
#[derive(clap::Subcommand)]
pub enum PackageCommand {
    /// Mirror packages from a spec file to an OCI registry
    Sync(sync::Sync),

    /// Check what would be mirrored without actually pushing (dry-run)
    Check(check::Check),

    /// Validate a mirror spec file
    Validate(validate::Validate),

    /// Pre-publish multi-runner test pipeline subcommands
    #[command(subcommand)]
    Pipeline(pipeline::PipelineCommand),
}

impl PackageCommand {
    pub async fn execute(&self, printer: &DataInterface, progress: &ProgressManager) -> Result<(), MirrorError> {
        match self {
            Self::Sync(cmd) => cmd.execute(printer, progress).await,
            Self::Check(cmd) => cmd.execute(printer).await,
            Self::Validate(cmd) => cmd.execute().await,
            Self::Pipeline(cmd) => cmd.execute(printer).await,
        }
    }
}
