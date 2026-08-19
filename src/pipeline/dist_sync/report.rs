// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The `dist sync` run report and its two renderings.
//!
//! Follows the local `OutputFormat` + free `report_*` function convention the
//! `registry sync` report already uses — no `Printable` trait is reachable
//! from this crate.

use ocx_lib::cli::{Cell, DataInterface};
use serde::Serialize;

use super::upload::UploadOutcome;
use crate::command::package::options::OutputFormat;

/// What the run did, at per-archive granularity.
#[derive(Debug, Default, Serialize)]
pub struct DistSyncReport {
    pub archives: Vec<ArchiveReport>,
    /// The sha256 of the rendered manifest — the name of its content-addressed
    /// snapshot, and what `dist.json.sha256` carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    pub counters: RunCounters,
    /// `true` when nothing was written or uploaded.
    pub dry_run: bool,
}

/// One release × target's outcome.
#[derive(Debug, Serialize)]
pub struct ArchiveReport {
    /// `<version>/<target>` — what identifies a row to an operator.
    pub name: String,
    pub outcome: ArchiveOutcome,
    /// The failure message for [`ArchiveOutcome::Failed`], absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What happened to one archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveOutcome {
    /// Downloaded into `output:` and verified against the manifest digest.
    Copied,
    /// Already in `output:` with a matching digest — a resumed or repeated
    /// run re-verifies rather than re-downloads.
    Skipped,
    /// Planned by `--dry-run`, which writes nothing.
    Planned,
    /// Download, verification, or layout expansion failed.
    Failed,
}

impl ArchiveOutcome {
    /// The plain-text label — the same spelling `rename_all` gives it in JSON,
    /// so the two renderings agree.
    fn label(self) -> &'static str {
        match self {
            ArchiveOutcome::Copied => "copied",
            ArchiveOutcome::Skipped => "skipped",
            ArchiveOutcome::Planned => "planned",
            ArchiveOutcome::Failed => "failed",
        }
    }
}

/// The run counters the summary line and the JSON report both carry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RunCounters {
    pub total: usize,
    pub copied: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Files this run PUT to the destination store.
    pub uploaded: usize,
    /// Files the destination store already held — the HEAD answered 2xx.
    pub already_present: usize,
}

impl RunCounters {
    /// Fold one upload outcome into the counters.
    ///
    /// A method rather than two `+= 1` sites, because the upload pass counts
    /// from two places now — a concurrent archive fan-out and the sequential
    /// publish-order tail — and the two must not drift.
    pub fn record(&mut self, outcome: UploadOutcome) {
        match outcome {
            UploadOutcome::Uploaded => self.uploaded += 1,
            UploadOutcome::Skipped => self.already_present += 1,
        }
    }
}

impl DistSyncReport {
    /// The summary line, verbatim shape:
    /// `"N total, M copied, K skipped, J failed; U uploaded, P already present"`.
    ///
    /// **A no-op run is never silent** — it prints the same line with zeroes,
    /// because silence is indistinguishable from "did not run" in a CI log.
    pub fn summary_line(&self) -> String {
        let RunCounters {
            total,
            copied,
            skipped,
            failed,
            uploaded,
            already_present,
        } = self.counters;
        format!(
            "{total} total, {copied} copied, {skipped} skipped, {failed} failed; \
             {uploaded} uploaded, {already_present} already present"
        )
    }

    /// Whether the run must exit non-zero.
    pub fn has_failures(&self) -> bool {
        self.counters.failed > 0
    }

    /// One message per failed archive, or `None` when the run is clean.
    ///
    /// `None` rather than an empty `Vec` so the caller cannot return
    /// `ExecutionFailed(vec![])` — an error carrying nothing, at exit 1, on a
    /// run that succeeded.
    ///
    /// No fallback branch is needed: the archive counters are *derived* from
    /// these same rows by [`RunCounters::from_archives`], so a counted failure
    /// with no row behind it cannot occur.
    pub fn failures(&self) -> Option<Vec<String>> {
        let errors: Vec<String> = self
            .archives
            .iter()
            .filter(|archive| archive.outcome == ArchiveOutcome::Failed)
            .map(|archive| format!("{}: {}", archive.name, archive.detail.as_deref().unwrap_or("failed")))
            .collect();

        (!errors.is_empty()).then_some(errors)
    }
}

impl RunCounters {
    /// Derive the archive tallies from the rows themselves.
    ///
    /// Derived rather than incremented alongside the rows, matching
    /// [`registry_sync`](crate::pipeline::registry_sync)'s own rule: two
    /// tallies of the same thing drift, and the drift surfaces as a summary
    /// line disagreeing with the table printed directly above it.
    ///
    /// `uploaded` and `already_present` stay incremented — no rows back them.
    pub fn from_archives(archives: &[ArchiveReport], uploaded: usize, already_present: usize) -> RunCounters {
        let count = |wanted: ArchiveOutcome| archives.iter().filter(|archive| archive.outcome == wanted).count();

        RunCounters {
            total: archives.len(),
            copied: count(ArchiveOutcome::Copied),
            skipped: count(ArchiveOutcome::Skipped),
            failed: count(ArchiveOutcome::Failed),
            uploaded,
            already_present,
        }
    }
}

/// Render the report — a table plus the summary line for
/// [`OutputFormat::Plain`], `serde_json::to_string_pretty` for
/// [`OutputFormat::Json`].
pub fn report_dist_sync(report: &DistSyncReport, format: OutputFormat, printer: &DataInterface) {
    match format {
        OutputFormat::Json => match serde_json::to_string_pretty(report) {
            Ok(json) => println!("{json}"),
            // Dropping this silently emits nothing at all under
            // `--format json`, and a clean run then exits 0 —
            // indistinguishable, to whatever parses the output, from a run
            // that produced no archives.
            Err(error) => tracing::error!("cannot render the run report as JSON: {error}"),
        },
        OutputFormat::Plain => {
            if !report.archives.is_empty() {
                let mut names = Vec::new();
                let mut outcomes = Vec::new();
                let mut details = Vec::new();
                for archive in &report.archives {
                    names.push(archive.name.clone());
                    outcomes.push(archive.outcome.label().to_string());
                    details.push(archive.detail.clone().unwrap_or_default());
                }
                printer.print_table(
                    &["Release".into(), "Outcome".into(), "Detail".into()],
                    &[names, outcomes, details].map(|c| c.into_iter().map(Cell::from).collect::<Vec<_>>()),
                );
                println!("---");
            }

            if let Some(sha256) = &report.manifest_sha256 {
                println!("manifest: dist/{sha256}.json");
            }
            println!("{}", report.summary_line());
        }
    }
}

#[cfg(test)]
#[path = "report/tests.rs"]
mod tests;
