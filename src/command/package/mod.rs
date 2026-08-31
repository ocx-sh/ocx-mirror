// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package` subcommand group.
//!
//! Mirrors upstream package releases into an OCI registry. Groups the one-shot
//! mirror verbs (`sync`, `check`, `validate`) with the pre-publish test
//! `pipeline`. Sibling namespace [`super::registry`] hosts whole-index
//! mirroring; see `adr_cli_namespace_restructure`.

mod check;
// `pub(crate)`: `pipeline::registry_sync::report` and `command::registry`
// render through the same `OutputFormat`, so both spellings of `--format` stay
// one type.
pub(crate) mod options;
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

/// The registry client every `package` verb reads and publishes through.
///
/// Replaces `ClientBuilder::from_env`, deleted in ocx v0.6.0 when the
/// plain-HTTP allowance became `[registries.<name>].insecure` and the client
/// stopped reading the environment for itself. The mirror parses no ocx
/// `Config`, so an empty one is the honest input: `insecure_hosts` over a
/// default `Config` is exactly `OCX_INSECURE_REGISTRIES`, which is what
/// `from_env` read — and it goes through the same
/// [`resolve_mirror_map`](ocx_lib::resolve_mirror_map) the CLI's
/// `Context::try_init` uses, so there is one `Config`→mirror-map transform
/// with one precedence rule and one plain-HTTP gate, not two.
///
/// Registry role only: an index-only `OCX_MIRRORS` entry must never rewrite
/// OCI traffic.
///
/// Deliberately not used by `pipeline::registry_sync::source_read_seam`, which
/// builds a bare client on purpose — a client-side mirror rewrite there would
/// dial a host other than the one its SSRF pre-flight approved.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] when the forwarded `OCX_MIRRORS` is
/// malformed, carries a non-string value, or names an `http://` mirror whose
/// host is not also in `OCX_INSECURE_REGISTRIES`. A failure aborts client
/// construction rather than degrading to an identity map — a silent degrade
/// would route reads to the firewall-blocked origin, the exact anti-goal
/// replace semantics exist to prevent.
pub(crate) fn registry_client() -> Result<ocx_lib::oci::Client, MirrorError> {
    let insecure = ocx_lib::env::insecure_registries();
    let env_mirrors = ocx_lib::env::mirrors().map_err(|error| MirrorError::ExecutionFailed(vec![error.to_string()]))?;
    let resolved = ocx_lib::resolve_mirror_map(&ocx_lib::Config::default(), env_mirrors, &insecure)
        .map_err(|error| MirrorError::ExecutionFailed(vec![error.to_string()]))?;
    Ok(ocx_lib::oci::ClientBuilder::new()
        .plain_http_registries(insecure)
        .mirrors(ocx_lib::oci::MirrorMap::new(resolved.registry))
        .build())
}
