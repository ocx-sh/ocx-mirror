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

use crate::command::package::pipeline::push::{
    ANNOUNCE_TIMEOUT, AnnounceReport, TagSource, invoke_announce, resolve_ocx_binary,
};
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

        // The dry run needs somewhere to throw the rebuilt entry away. It goes
        // under the pipeline's own `.ocx-mirror` work dir rather than the shared
        // system temp dir: a predictable `/tmp/<fixed-name>` is a path any other
        // local user can pre-create as a symlink, and `--out` writes through it.
        // `tempfile` would solve that too but is a dev-dependency here, and this
        // needs no randomness once the directory is not shared. `ocx package
        // announce --out` creates it.
        let out = self
            .dry_run
            .then(|| std::path::PathBuf::from(".ocx-mirror").join(format!("announce-dry-run-{}", std::process::id())));

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

        let line = report_line(
            self.dry_run,
            &report,
            config,
            &format!("{}/{}", spec.target.registry, spec.target.repository),
        );
        log::info!("{line}");

        Ok(())
    }
}

/// The single line this command logs for `report`.
///
/// A dry run and a real run must never be able to print the same text. The
/// real path's `updated` and its `no pull request reported` are both
/// legitimate outcomes of a run that *did* curate, so a dry run reusing that
/// wording leaves a CI log in which nothing happened indistinguishable from
/// one in which the index moved.
///
/// The mode goes in the tag, not a suffix, so the distinction survives a
/// truncated or grepped log. Every arm carries it, `unchanged` included: a
/// marker that appears only in some arms makes its absence meaningless, and
/// "the index already carries every tag" reads as a settled fact about a
/// reconciliation that a dry run never attempted.
///
/// The real-run arms are the wording other tooling and the run summary read —
/// frozen here, and asserted byte-for-byte by the tests below.
fn report_line(dry_run: bool, report: &AnnounceReport, config: &spec::AnnounceConfig, target: &str) -> String {
    let package = &config.package;
    let index_repo = &config.index_repo;
    let status = report.status.as_str();

    if dry_run {
        // One arm for every status: a dry run curates nothing whatever the
        // index would have said, and `status` still tells the operator which
        // way a real run would go.
        return format!(
            "[announce:dry-run] {package} — nothing was curated and no pull request was opened; \
             a real run would report {status} against {index_repo}"
        );
    }

    // Same reporting shape as the push job's `run_announce`: `unchanged` with
    // no pull request is the no-op, and must not read as a curation that
    // happened.
    match (status, report.pull_request_url.as_deref()) {
        ("unchanged", None) => format!("[announce] {package} — index already carries every tag {target} holds"),
        (status, pull_request_url) => {
            let pull_request_url = pull_request_url.unwrap_or("no pull request reported");
            format!("[announce] {package} → {index_repo} ({status}, {pull_request_url})")
        }
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

    fn config() -> spec::AnnounceConfig {
        spec::AnnounceConfig {
            package: "bazelbuild/buildifier".to_string(),
            fork: "ocx-contrib/index".to_string(),
            index_repo: "ocx-sh/index".to_string(),
        }
    }

    fn report(status: &str, pull_request_url: Option<&str>) -> AnnounceReport {
        AnnounceReport {
            status: status.to_string(),
            pull_request_url: pull_request_url.map(str::to_string),
        }
    }

    const TARGET: &str = "ghcr.io/ocx-sh/bazelbuild-buildifier";

    /// The defect: a dry run printed the line a real run prints. Asserted as a
    /// *pair* over every status the announce reports — a test that only pinned
    /// the dry-run wording would still pass if the real path drifted onto it.
    #[test]
    fn a_dry_run_and_a_real_run_never_report_the_same_line() {
        for status in ["updated", "unchanged"] {
            for pull_request_url in [None, Some("https://github.com/ocx-sh/index/pull/7")] {
                let report = report(status, pull_request_url);
                let dry = report_line(true, &report, &config(), TARGET);
                let real = report_line(false, &report, &config(), TARGET);

                assert_ne!(dry, real, "status={status}, pull_request_url={pull_request_url:?}");
                assert!(
                    dry.starts_with("[announce:dry-run] "),
                    "a dry run must name its mode up front, got: {dry}"
                );
                assert!(
                    !real.contains("dry-run") && !real.contains("dry run"),
                    "the real path must not claim to be a dry run, got: {real}"
                );
            }
        }
    }

    /// `unchanged` + no pull request is the arm that already read as a settled
    /// fact in both modes. It is distinguished too: the dry run reports what a
    /// real run *would* say, never that the index was found current.
    #[test]
    fn the_dry_run_disclaims_curation_on_every_status() {
        for status in ["updated", "unchanged"] {
            let line = report_line(true, &report(status, None), &config(), TARGET);
            assert_eq!(
                line,
                format!(
                    "[announce:dry-run] bazelbuild/buildifier — nothing was curated and no pull request \
                     was opened; a real run would report {status} against ocx-sh/index"
                ),
                "got: {line}"
            );
        }
    }

    /// The real-run wording is the contract other tooling reads. Frozen so the
    /// fix above cannot be "kept" by quietly moving the real path instead.
    #[test]
    fn the_real_run_wording_is_unchanged() {
        assert_eq!(
            report_line(false, &report("unchanged", None), &config(), TARGET),
            format!("[announce] bazelbuild/buildifier — index already carries every tag {TARGET} holds")
        );
        assert_eq!(
            report_line(false, &report("unchanged", Some("https://x/1")), &config(), TARGET),
            "[announce] bazelbuild/buildifier → ocx-sh/index (unchanged, https://x/1)"
        );
        assert_eq!(
            report_line(false, &report("updated", None), &config(), TARGET),
            "[announce] bazelbuild/buildifier → ocx-sh/index (updated, no pull request reported)"
        );
    }
}
