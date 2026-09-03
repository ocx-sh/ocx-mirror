// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The run report and its two renderings (C-042, and the output half of
//! C-043).
//!
//! Follows the local `OutputFormat` + free `report_*` function convention
//! rather than a `Printable` impl — no `Printable` trait is reachable from this
//! crate, it lives in `ocx_cli`.

use ocx_lib::cli::{Cell, DataInterface, human_bytes};
use serde::Serialize;

use crate::command::package::options::OutputFormat;

/// What the run did, at per-source and per-package granularity (C-042).
#[derive(Debug, Default, Serialize)]
pub struct RegistrySyncReport {
    pub sources: Vec<SourceReport>,
    pub counters: RunCounters,
    /// Bytes a real run would transfer — `Some` only under `--dry-run`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_bytes: Option<u64>,
}

/// One source's slice of the run.
#[derive(Debug, Serialize)]
pub struct SourceReport {
    /// The source's `as:` value — its output subtree and `{registry}`
    /// expansion.
    pub as_name: String,
    /// Whether this source's whole pass was skipped by the short-circuit.
    pub short_circuited: bool,
    pub packages: Vec<PackageReport>,
}

/// One package's outcome.
#[derive(Debug, Serialize)]
pub struct PackageReport {
    /// The catalog key — the logical `<ns>/<pkg>` name.
    pub name: String,
    pub outcome: PackageOutcome,
    /// The failure message for [`PackageOutcome::Failed`], absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the signature sweeps carried for this package (C-064).
    #[serde(flatten)]
    pub signatures: SignatureCounts,
}

/// The three signature counters C-064 carries into both renderings.
///
/// A struct rather than three fields on [`PackageReport`] so the plain table
/// and the JSON payload cannot drift apart on which three they mean, and
/// `#[serde(flatten)]`ed so the JSON shape stays flat for whatever parses it.
///
/// **Zeros are emitted, never omitted.** "No signatures carried" and "this
/// mirror does not carry signatures" are different facts, and a field that
/// disappears at zero makes them indistinguishable to a script.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SignatureCounts {
    /// Referrer manifests carried across.
    pub referrers_copied: usize,
    /// Cosign sidecar tags carried across.
    pub sidecars_copied: usize,
    /// Sidecar tags the destination already held at a different digest, so
    /// they were skipped rather than overwritten.
    pub sidecar_conflicts: usize,
}

/// What happened to one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageOutcome {
    /// Content transferred and the root written.
    Copied,
    /// Already present at the destination with matching tag→digest pairs.
    Skipped,
    /// An aggregating failure; the run continues under `on_error: continue`.
    Failed,
}

impl PackageOutcome {
    /// The plain-text table label — the same spelling
    /// `#[serde(rename_all = "snake_case")]` gives it in JSON, so the two
    /// renderings agree.
    fn label(self) -> &'static str {
        match self {
            PackageOutcome::Copied => "copied",
            PackageOutcome::Skipped => "skipped",
            PackageOutcome::Failed => "failed",
        }
    }
}

/// The four run counters the summary line and the JSON report both carry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RunCounters {
    pub total: usize,
    pub copied: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl RegistrySyncReport {
    /// The summary line, verbatim shape:
    /// `"N total, M copied, K skipped, J failed"`.
    ///
    /// **A no-op run is never silent** — it prints the same line with
    /// `0 copied`, because silence is indistinguishable from "did not run" in
    /// a CI log.
    pub fn summary_line(&self) -> String {
        let RunCounters {
            total,
            copied,
            skipped,
            failed,
        } = self.counters;
        format!("{total} total, {copied} copied, {skipped} skipped, {failed} failed")
    }

    /// Whether the run must exit non-zero.
    pub fn has_failures(&self) -> bool {
        self.counters.failed > 0
    }
}

/// Render the report — a table plus the summary line for
/// [`OutputFormat::Plain`], `serde_json::to_string_pretty` for
/// [`OutputFormat::Json`].
pub fn report_registry_sync(report: &RegistrySyncReport, format: OutputFormat, printer: &DataInterface) {
    match format {
        OutputFormat::Json => match serde_json::to_string_pretty(report) {
            Ok(json) => println!("{json}"),
            // Dropping this silently emits **nothing at all** under
            // `--format json`, and a run whose packages all succeeded then
            // exits 0 — indistinguishable, to whatever parses the output, from
            // a run that produced no packages. Every field here serializes
            // infallibly today; the branch exists so that stops being an
            // unstated assumption the moment one does not.
            Err(error) => tracing::error!("cannot render the run report as JSON: {error}"),
        },
        OutputFormat::Plain => {
            // Per-source granularity the package table below cannot show: a
            // short-circuited source contributes zero package rows by
            // construction (C-039), which would otherwise be indistinguishable
            // from "matched nothing" — state it explicitly instead.
            for source in &report.sources {
                if source.short_circuited {
                    println!("{}: unchanged since the last run — nothing to compare", source.as_name);
                }
            }

            let mut sources = Vec::new();
            let mut names = Vec::new();
            let mut outcomes = Vec::new();
            let mut referrers = Vec::new();
            let mut sidecars = Vec::new();
            let mut conflicts = Vec::new();
            let mut details = Vec::new();

            for source in &report.sources {
                for package in &source.packages {
                    sources.push(source.as_name.clone());
                    names.push(package.name.clone());
                    outcomes.push(package.outcome.label().to_string());
                    referrers.push(package.signatures.referrers_copied.to_string());
                    sidecars.push(package.signatures.sidecars_copied.to_string());
                    conflicts.push(package.signatures.sidecar_conflicts.to_string());
                    details.push(package.detail.clone().unwrap_or_default());
                }
            }

            // A run that matched nothing — or a fully short-circuited re-run,
            // S-002 — has zero package rows. An empty table (header, no data)
            // reads as broken output, not "nothing changed"; skip it and let
            // the summary line below be the explanation.
            if !names.is_empty() {
                printer.print_table(
                    &[
                        "Source".into(),
                        "Package".into(),
                        "Outcome".into(),
                        "Referrers".into(),
                        "Sidecars".into(),
                        "Conflicts".into(),
                        "Detail".into(),
                    ],
                    &[sources, names, outcomes, referrers, sidecars, conflicts, details]
                        .map(|c| c.into_iter().map(Cell::from).collect::<Vec<_>>()),
                );
                println!("---");
            }

            println!("{}", report.summary_line());

            // C-043's output half. JSON keeps the raw integer via
            // `estimated_bytes`'s own `Serialize` — this plain rendering is
            // the one place `human_bytes` applies.
            if let Some(bytes) = report.estimated_bytes {
                println!("estimated transfer: {}", human_bytes(bytes as i64));
            }
        }
    }
}

#[cfg(test)]
#[path = "report/tests.rs"]
mod tests;
