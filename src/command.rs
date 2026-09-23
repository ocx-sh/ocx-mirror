// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

pub(crate) mod dist;
pub(crate) mod package;
pub(crate) mod registry;
#[cfg(feature = "jsonschema")]
mod schema;
mod version;

use ocx_console::DataInterface;
use ocx_console::progress::ProgressManager;

use crate::error::MirrorError;
use crate::pipeline::options::OutputFormat;

#[derive(clap::Subcommand)]
pub enum Command {
    /// Mirror upstream package releases into an OCI registry
    #[command(subcommand)]
    Package(package::PackageCommand),

    /// Mirror whole OCX index sources into a corporate OCI registry
    #[command(subcommand)]
    Registry(registry::RegistryCommand),

    /// Mirror the OCX distribution (release archives + dist.json) into a
    /// generic store
    #[command(subcommand)]
    Dist(dist::DistCommand),

    /// Generate JSON Schema for mirror types
    #[cfg(feature = "jsonschema")]
    Schema(schema::Schema),

    /// Print the ocx-mirror version and the build provenance baked into it
    Version(version::Version),
}

impl Command {
    /// Folds the root `--format` / `--json` into the chosen command.
    ///
    /// The root group is the documented spelling (ocx's, from `ocx_console`);
    /// the per-command `--format` flags predate it and keep working byte for
    /// byte. Where both are given, JSON wins if either asks for it — a
    /// defaulted per-command flag cannot tell `plain` typed from `plain`
    /// assumed. An absent root flag changes nothing.
    pub fn apply_format(&mut self, global: Option<ocx_console::FormatMode>) {
        let Some(global) = global.map(|mode| match mode {
            ocx_console::FormatMode::Json => OutputFormat::Json,
            ocx_console::FormatMode::Plain => OutputFormat::Plain,
        }) else {
            return;
        };
        match self {
            Self::Package(cmd) => cmd.apply_format(global),
            Self::Registry(registry::RegistryCommand::Sync(cmd)) => {
                cmd.options.format = with_global(cmd.options.format, global);
            }
            Self::Dist(dist::DistCommand::Sync(cmd)) => {
                cmd.options.format = with_global(cmd.options.format, global);
            }
            #[cfg(feature = "jsonschema")]
            Self::Schema(_) => {}
            Self::Version(cmd) => cmd.format = Some(global),
        }
    }

    pub async fn execute(&self, printer: &DataInterface, progress: &ProgressManager) -> Result<(), MirrorError> {
        match self {
            Self::Package(cmd) => cmd.execute(printer, progress).await,
            Self::Registry(cmd) => cmd.execute(printer).await,
            Self::Dist(cmd) => cmd.execute(printer).await,
            #[cfg(feature = "jsonschema")]
            Self::Schema(cmd) => cmd.execute().await,
            Self::Version(cmd) => cmd.execute(printer),
        }
    }
}

/// A defaulted per-command `--format` under a root `--format`: JSON if either
/// asks for it.
fn with_global(own: OutputFormat, global: OutputFormat) -> OutputFormat {
    match global {
        OutputFormat::Json => OutputFormat::Json,
        OutputFormat::Plain => own,
    }
}

#[cfg(test)]
#[path = "command/tests.rs"]
mod tests;
