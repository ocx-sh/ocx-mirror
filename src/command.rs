// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

pub(crate) mod package;
pub(crate) mod registry;
#[cfg(feature = "jsonschema")]
mod schema;

use ocx_lib::cli::DataInterface;
use ocx_lib::cli::progress::ProgressManager;

use crate::error::MirrorError;

#[derive(clap::Subcommand)]
pub enum Command {
    /// Mirror upstream package releases into an OCI registry
    #[command(subcommand)]
    Package(package::PackageCommand),

    /// Mirror whole OCX index sources into a corporate OCI registry
    #[command(subcommand)]
    Registry(registry::RegistryCommand),

    /// Generate JSON Schema for mirror types
    #[cfg(feature = "jsonschema")]
    Schema(schema::Schema),
}

impl Command {
    pub async fn execute(&self, printer: &DataInterface, progress: &ProgressManager) -> Result<(), MirrorError> {
        match self {
            Self::Package(cmd) => cmd.execute(printer, progress).await,
            Self::Registry(cmd) => cmd.execute(printer).await,
            #[cfg(feature = "jsonschema")]
            Self::Schema(cmd) => cmd.execute().await,
        }
    }
}
