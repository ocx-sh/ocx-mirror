// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline sign` — sign what the target repository
//! already holds.
//!
//! The one leg of `adr_mirror_signing.md` D2 that cannot ride along with a
//! push: everything published before `sign:` reached the spec carries no
//! signature, and re-pushing to get one would rewrite digests consumers have
//! pinned. This signs the published content in place.
//!
//! Convergent and repeatable — a re-run signs only what is still unsigned, so
//! it is safe on a timer and safe to interrupt. `--force` is the deliberate
//! exception: it signs regardless, which is how a second identity joins an
//! existing signature.
//!
//! `--identity` / `--issuer` narrow what "unsigned" means to *not signed by
//! this signer*, so a subject carrying only somebody else's signature is
//! signed again — the rotation case. Both are exact matches, both must hold on
//! one candidate, and `--force` outranks both.
//!
//! No generated workflow renders this command: it is an operator verb, run by
//! hand or from the four-line job `docs/reference/cli.md` documents.
//!
//! # Errors
//!
//! - [`MirrorError::SpecNotFound`] / [`MirrorError::SpecInvalid`] from
//!   `load_spec`.
//! - [`MirrorError::SpecUsageError`] (exit 64) when the spec carries no
//!   `sign:` block — there is nothing to sign *with*.
//! - [`MirrorError::SignMaterialMissing`] (exit 78) when a `sign:` ref names
//!   material that cannot be reached.
//! - [`MirrorError::TargetError`] (exit 69) when the target registry's tag
//!   list or a published manifest cannot be read — the backfill must never
//!   read a failed listing as "nothing to sign".
//! - [`MirrorError::SignFailed`] carrying the **worst** classified child exit
//!   among the failed rows (C-069). Every other row is still in the report.

use std::path::PathBuf;

use ocx_lib::cli::{Cell, DataInterface};
use ocx_lib::publisher::Publisher;

use crate::command::package::options::OutputFormat;
use crate::error::MirrorError;
use crate::pipeline::ocx_cli::sign::resolve_sign_from_env;
use crate::pipeline::sign_backfill::{Backfill, BatchReport, ItemStatus, SignFilter, SkipReason, plan_targets};
use crate::pipeline::target_registry;
use crate::spec;

/// `ocx-mirror package pipeline sign` subcommand.
#[derive(clap::Parser)]
pub struct Sign {
    /// Path to the mirror spec file
    #[arg(default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Report the filter verdict per subject and sign nothing
    #[arg(long)]
    pub dry_run: bool,

    /// Sign every subject regardless of any existing signature
    #[arg(long)]
    pub force: bool,

    /// Count only signatures whose certificate identity is this exact value
    #[arg(long, value_name = "SAN")]
    pub identity: Option<String>,

    /// Count only signatures whose certificate OIDC issuer is this exact value
    #[arg(long, value_name = "URL")]
    pub issuer: Option<String>,

    /// Output format
    #[arg(long, value_enum, default_value = "plain")]
    pub format: OutputFormat,
}

impl Sign {
    pub async fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_spec(&self.spec).await?;

        // Refused here rather than deep in the resolver so the message names
        // the missing block, not a missing credential: a mirror with no
        // `sign:` has nothing to sign *with*, and defaulting to some ambient
        // provider would publish signatures the spec never asked for.
        let Some(sign_config) = spec.sign.as_ref() else {
            return Err(MirrorError::SpecUsageError(format!(
                "{} carries no `sign:` block, so there is no signing material to back a backfill",
                self.spec.display()
            )));
        };
        // Once per run, before the first child (C-054): a missing variable
        // fails here with one message rather than once per subject.
        let resolved = resolve_sign_from_env(Some(sign_config))?;
        let Some(resolved) = resolved else {
            return Err(MirrorError::SpecUsageError(format!(
                "{} carries no `sign:` block, so there is no signing material to back a backfill",
                self.spec.display()
            )));
        };

        let identifier = ocx_lib::oci::Identifier::new_registry(&spec.target.repository, &spec.target.registry);
        let publisher = Publisher::new(crate::command::package::registry_client()?);

        // Fail-safe, exactly as discover is (issue #157): only an
        // authoritative "repository not found" reads as an empty repository.
        // A transient listing failure that read as "nothing published" would
        // report a green no-op run over a repository full of unsigned content.
        let tags = target_registry::list_target_tags(&publisher, &identifier).await?;
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let published = target_registry::fetch_signing_subjects(&publisher, &identifier, &tag_refs).await?;

        let (indexes, platforms) = plan_targets(&published);

        // Over the same registry the tag list came from, so discovery
        // inherits the credential ladder and the plain-HTTP policy the copy
        // engine already resolves rather than picking its own.
        let transport = ocx_lib::oci::client::native_transport(
            crate::pipeline::registry_copy::build_destination_client(&spec.target).await,
            ocx_lib::auth::Auth::new(),
        );

        let report = Backfill {
            target: &spec.target,
            sign: &resolved,
            transport: transport.as_ref(),
            filter: SignFilter {
                force: self.force,
                identity: self.identity.clone(),
                issuer: self.issuer.clone(),
            },
            dry_run: self.dry_run,
            max_retries: spec.concurrency.max_retries,
        }
        .run(&indexes, &platforms)
        .await;

        report_backfill(&report, self.format, printer);
        warn_if_cancelled(&report);

        if report.exit_code() == ocx_lib::cli::ExitCode::Success {
            return Ok(());
        }
        let summary = report.summary();
        Err(MirrorError::SignFailed {
            // The batch's own identity: the repository, and how much of it
            // failed. Never a secret — a resolved ref reaches neither an argv
            // word nor a message (C-054).
            target: format!(
                "{}/{} ({} of {} subjects)",
                spec.target.registry, spec.target.repository, summary.failed, summary.total
            ),
            code: i32::from(summary.exit_code),
        })
    }
}

/// Render the report — a table plus the summary line for
/// [`OutputFormat::Plain`], the pinned envelope for [`OutputFormat::Json`].
///
/// Under `--format json` stdout carries the envelope and nothing else
/// (CLI-02): the per-row narration below is the *other* rendering, not a
/// preamble to this one.
fn report_backfill(report: &BatchReport, format: OutputFormat, printer: &DataInterface) {
    match format {
        OutputFormat::Json => match serde_json::to_string_pretty(&report.envelope()) {
            Ok(json) => println!("{json}"),
            // Said, not swallowed: emitting nothing under `--format json` is
            // indistinguishable to a parser from a run that considered no
            // subjects, and an all-skipped run exits 0 either way.
            Err(error) => ocx_lib::log::error!("cannot render the backfill report as JSON: {error}"),
        },
        OutputFormat::Plain => {
            if !report.items.is_empty() {
                let mut tags = Vec::new();
                let mut platforms = Vec::new();
                let mut statuses = Vec::new();
                let mut details = Vec::new();

                for item in &report.items {
                    tags.push(item.tag.clone());
                    // unwrap_or_default: an index row has no platform; the cell is blank.
                    platforms.push(item.platform.clone().unwrap_or_default());
                    statuses.push(status_label(item.status).to_string());
                    details.push(detail(item));
                }

                printer.print_table(
                    &["Tag".into(), "Platform".into(), "Status".into(), "Detail".into()],
                    &[tags, platforms, statuses, details].map(|c| c.into_iter().map(Cell::from).collect::<Vec<_>>()),
                );
                println!("---");
            }

            // Printed even for an empty run: silence is indistinguishable from
            // "did not run" in a CI log, and a repository whose every subject
            // is already signed is the ordinary steady state.
            let summary = report.summary();
            println!(
                "{} total, {} signed, {} skipped, {} failed",
                summary.total, summary.succeeded, summary.skipped, summary.failed
            );
        }
    }
}

/// Say on stderr that the run was interrupted, and how much it never reached.
///
/// An interrupted pass that hit no failures exits **0** — the pinned table has
/// no cancellation code and PKG-24 forbids inventing one — so `summary.status`
/// is the only place the interruption appears, and a plain-text user never
/// sees it. Without this line the run claims success to everyone not parsing
/// `--format json`.
///
/// `eprintln!` rather than a `log` macro on purpose: this must not be
/// suppressible by `--log-level`, and it must reach the same terminal the
/// operator just pressed Ctrl-C in. It is not part of the `--format json`
/// payload either way — that is stdout, and this is not (CLI-01, CLI-02).
fn warn_if_cancelled(report: &BatchReport) {
    let unattempted = report
        .items
        .iter()
        .filter(|item| item.reason == Some(SkipReason::Cancelled))
        .count();
    if unattempted == 0 {
        return;
    }
    eprintln!(
        "warning: interrupted — {unattempted} of {} subjects were never attempted; \
         re-run to sign them (this pass signed only what it reached)",
        report.items.len()
    );
}

/// The plain-table label — the same spelling `#[serde(rename_all)]` gives it
/// in JSON, so the two renderings cannot drift.
fn status_label(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Succeeded => "succeeded",
        ItemStatus::Failed => "failed",
        ItemStatus::Skipped => "skipped",
    }
}

/// The `Detail` column: why a row was skipped, or what its failure was.
fn detail(item: &crate::pipeline::sign_backfill::ItemReport) -> String {
    if let Some(error) = item.error.as_ref() {
        return format!("exit {}", error.exit);
    }
    match item.reason {
        Some(SkipReason::AlreadySigned) => "already signed".to_string(),
        Some(SkipReason::DryRun) => "would sign".to_string(),
        Some(SkipReason::Cancelled) => "not attempted (interrupted)".to_string(),
        None => item.subject.clone(),
    }
}
