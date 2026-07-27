// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline announce` — announce every tag the registry
//! currently holds into the index.
//!
//! The push job announces only what *its own run* published, which is the right
//! scope for a mirror that had `announce:` from the start. It cannot catch up a
//! mirror that published first and opted in later: no future run's tag set ever
//! covers the backlog, so every run reports `nothing_to_announce` indefinitely.
//!
//! This command closes that gap by listing the physical repository's tags and
//! unioning them onto the committed index root. It is additive — like
//! `--tags-from-file` it can never drop a committed tag, and yank markers
//! survive — so it is safe to dispatch against a mirror that is already
//! current.
//!
//! # Errors
//!
//! - [`MirrorError::SpecNotFound`] / [`MirrorError::SpecInvalid`] from
//!   `load_spec`.
//! - [`MirrorError::SpecUsageError`] (exit 64) when the spec has no `announce:`
//!   block — there is no index package to announce into.
//! - [`MirrorError::ExecutionFailed`] when the `ocx package announce`
//!   subprocess fails or reports no readable JSON.

use std::path::PathBuf;

use ocx_lib::cli::DataInterface;
use ocx_lib::log;

use crate::command::package::pipeline::push::{ANNOUNCE_TIMEOUT, TagSource, invoke_announce, resolve_ocx_binary};
use crate::error::MirrorError;
use crate::spec;

/// `ocx-mirror package pipeline announce` subcommand.
#[derive(clap::Parser)]
pub struct Announce {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Report what the announce would change without opening an index pull
    /// request: the rebuilt entry is written to a temporary directory and
    /// discarded.
    #[arg(long)]
    pub dry_run: bool,
}

impl Announce {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_spec(&self.spec).await?;

        let Some(config) = spec.announce.as_ref() else {
            return Err(MirrorError::SpecUsageError(format!(
                "{} has no `announce:` block — add one naming the logical index package \
                 before announcing from the registry",
                self.spec.display()
            )));
        };

        let ocx_binary = resolve_ocx_binary().map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;

        // ponytail: a pid-named directory under the system temp dir, removed on
        // the way out. `tempfile` is a dev-dependency here and this is its only
        // runtime caller — not worth widening the binary's dependency graph.
        // `ocx package announce --out` creates the directory itself.
        let out = self
            .dry_run
            .then(|| std::env::temp_dir().join(format!("ocx-mirror-announce-{}", std::process::id())));

        let result = invoke_announce(
            config,
            &TagSource::FromRegistry,
            out.as_deref(),
            &ocx_binary,
            ANNOUNCE_TIMEOUT,
        )
        .await;

        if let Some(directory) = &out {
            let _ = tokio::fs::remove_dir_all(directory).await;
        }

        let report = result.map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;

        // Same reporting shape as the push job's `run_announce`: `unchanged`
        // with no pull request is the no-op, and must not read as a curation
        // that happened.
        match (report.status.as_str(), report.pull_request_url.as_deref()) {
            ("unchanged", None) => log::info!(
                "[announce] {} — index already carries every tag {} holds",
                config.package,
                format_args!("{}/{}", spec.target.registry, spec.target.repository),
            ),
            (status, pull_request_url) => log::info!(
                "[announce] {} → {} ({status}, {})",
                config.package,
                config.index_repo,
                pull_request_url.unwrap_or("no pull request reported"),
            ),
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocx_lib::cli::ExitCode;

    /// A mirror without `announce:` has no index package to announce into.
    /// Exit 64 (usage), not 65 — the spec is valid, the command is wrong for it.
    #[tokio::test]
    async fn a_spec_without_an_announce_block_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("mirror.yml");
        let fixture = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        std::fs::copy(fixture, &spec_path).unwrap();

        let cmd = Announce {
            spec: spec_path,
            dry_run: true,
        };
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let error = cmd.execute(&printer).await.expect_err("no announce block must fail");

        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "got: {error}");
    }
}
