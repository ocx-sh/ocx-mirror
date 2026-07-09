// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

// `pub(crate)`: `pipeline::python_push` (outside this subtree) reaches
// `command::package::target_registry`'s fail-safe tag-listing helper for the
// wheel-registration tag-exists check.
pub(crate) mod package;
// Reserved namespace — registry-to-registry mirroring. Documented placeholder
// only; no `Registry` arm on `Command` until the first subcommand lands.
// See .claude/artifacts/adr_cli_namespace_restructure.md.
mod registry;
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

    /// Generate JSON Schema for mirror types
    #[cfg(feature = "jsonschema")]
    Schema(schema::Schema),
}

impl Command {
    pub async fn execute(&self, printer: &DataInterface, progress: &ProgressManager) -> Result<(), MirrorError> {
        match self {
            Self::Package(cmd) => cmd.execute(printer, progress).await,
            #[cfg(feature = "jsonschema")]
            Self::Schema(cmd) => cmd.execute().await,
        }
    }
}
