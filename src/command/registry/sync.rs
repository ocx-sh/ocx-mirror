// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror registry sync` — copy whole index sources into a corporate
//! registry and write the servable index tree that points at them (C-045).

use std::path::PathBuf;

use ocx_lib::cli::DataInterface;

use super::options::RegistrySyncOptions;
use crate::error::MirrorError;
use crate::pipeline::registry_sync::execute_registry_sync;
use crate::pipeline::registry_sync::report::{PackageOutcome, RegistrySyncReport, report_registry_sync};
use crate::spec;

#[derive(clap::Args)]
pub struct Sync {
    // Optional positional with a default, which is new to this crate:
    // `package sync <SPEC>` is a required bare positional because a package
    // mirror repo holds many specs, while a corporate registry mirror repo
    // holds exactly one. No `long`, so it stays positional; a bare `PathBuf`,
    // not an `Option<PathBuf>`.
    /// Path to the registry spec
    #[arg(default_value = "./registry.yml")]
    pub spec: PathBuf,

    #[clap(flatten)]
    pub options: RegistrySyncOptions,
}

impl Sync {
    /// Load the spec, run the sync, render the report, and map the outcome to
    /// an exit code.
    ///
    /// # Errors
    ///
    /// Whatever `load_registry_spec` and `execute_registry_sync` return —
    /// C-040's whole-run abort class propagates verbatim, and a run with any
    /// per-package failure returns
    /// [`MirrorError::ExecutionFailed`](crate::error::MirrorError::ExecutionFailed)
    /// (exit 1).
    ///
    /// The two classes reach the exit code by different routes on purpose. A
    /// whole-run abort short-circuits at its `?` and **renders no report**,
    /// because there is none — the run stopped before it could count anything.
    /// Per-package failures render the report first and classify after, so the
    /// operator sees which packages failed rather than only that some did.
    pub async fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_registry_spec(&self.spec).await?;
        let report = execute_registry_sync(&spec, &self.options).await?;
        report_registry_sync(&report, self.options.format, printer);

        match failed_packages(&report) {
            None => Ok(()),
            Some(errors) => Err(MirrorError::ExecutionFailed(errors)),
        }
    }
}

/// One message per failed package, or `None` when the run is clean.
///
/// Split out of [`Sync::execute`] because it is the whole of C-045's
/// exit-code decision and the only part of the verb reachable without a
/// registry: `execute` itself needs a live source and destination, so this is
/// where the classification gets a test.
///
/// `None` rather than an empty `Vec` so the caller cannot accidentally return
/// `ExecutionFailed(vec![])` — an error carrying nothing, at exit 1, on a run
/// that succeeded.
///
/// The counter is the authority on *whether* the run failed; the package rows
/// only supply the *wording*. They can disagree — a failure counted against no
/// package row — and the counter wins, so the summary line stands in rather
/// than letting the error carry nothing.
fn failed_packages(report: &RegistrySyncReport) -> Option<Vec<String>> {
    if !report.has_failures() {
        return None;
    }
    let mut errors = Vec::new();
    for source in &report.sources {
        for package in &source.packages {
            if package.outcome == PackageOutcome::Failed {
                errors.push(format!(
                    "{}/{}: {}",
                    source.as_name,
                    package.name,
                    package.detail.as_deref().unwrap_or("failed")
                ));
            }
        }
    }
    if errors.is_empty() {
        errors.push(report.summary_line());
    }
    Some(errors)
}

#[cfg(test)]
#[path = "sync/tests.rs"]
mod tests;
