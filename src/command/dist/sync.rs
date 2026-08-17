// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror dist sync` — mirror the OCX distribution into a generic store.

use std::path::PathBuf;

use ocx_lib::cli::DataInterface;

use super::options::DistSyncOptions;
use crate::error::MirrorError;
use crate::pipeline::dist_sync::execute_dist_sync;
use crate::pipeline::dist_sync::report::report_dist_sync;
use crate::spec;

#[derive(clap::Args)]
pub struct Sync {
    // Optional positional with a default, matching `registry sync`: a
    // distribution mirror repository holds exactly one spec, so the path is
    // sugar rather than the required selector `package sync <SPEC>` needs.
    /// Path to the distribution spec
    #[arg(default_value = "./dist.yml")]
    pub spec: PathBuf,

    #[clap(flatten)]
    pub options: DistSyncOptions,
}

impl Sync {
    /// Load the spec, run the sync, render the report, and map the outcome to
    /// an exit code.
    ///
    /// # Errors
    ///
    /// Whatever `load_dist_spec` and `execute_dist_sync` return — a whole-run
    /// abort propagates verbatim, and a run with any per-archive failure
    /// returns
    /// [`MirrorError::ExecutionFailed`](crate::error::MirrorError::ExecutionFailed)
    /// (exit 1).
    ///
    /// The two classes reach the exit code by different routes on purpose. A
    /// whole-run abort short-circuits at its `?` and **renders no report**,
    /// because there is none. Per-archive failures render the report first and
    /// classify after, so the operator sees which releases failed rather than
    /// only that some did.
    pub async fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_dist_spec(&self.spec).await?;
        let report = execute_dist_sync(&spec, self.options.dry_run).await?;
        report_dist_sync(&report, self.options.format, printer);

        match report.failures() {
            None => Ok(()),
            Some(errors) => Err(MirrorError::ExecutionFailed(errors)),
        }
    }
}
