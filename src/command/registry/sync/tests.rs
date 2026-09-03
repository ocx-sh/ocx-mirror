// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-045 — the argument grammar of `ocx-mirror registry sync`.
//!
//! The optional-positional-with-a-default shape is new to this crate: every
//! other spec argument is either a required bare positional
//! (`package sync <SPEC>`) or a named flag with a default (`--spec`, all seven
//! `pipeline` subcommands). Nothing else pins it, and getting it wrong is
//! silent — a required positional turns the bare invocation into a usage
//! error, and an `Option<PathBuf>` compiles fine while defaulting to `None`.

use clap::Parser;

use super::super::super::Command;
use super::super::RegistryCommand;

/// Parses through the real `Command` tree rather than `Sync` alone, so the
/// test covers the `registry sync` wiring as well as the default.
#[derive(Parser)]
struct Harness {
    #[command(subcommand)]
    command: Command,
}

fn parse(arguments: &[&str]) -> super::Sync {
    let harness = Harness::try_parse_from(arguments).expect("parse");
    match harness.command {
        Command::Registry(RegistryCommand::Sync(sync)) => sync,
        _ => panic!("`registry sync` parsed as a different command"),
    }
}

#[test]
fn the_spec_argument_defaults_to_registry_yml() {
    let sync = parse(&["ocx-mirror", "registry", "sync"]);

    assert_eq!(sync.spec, std::path::Path::new("./registry.yml"));
}

#[test]
fn a_positional_spec_path_overrides_the_default() {
    // Positional, not `--spec`: a corporate registry mirror repo holds exactly
    // one spec, so naming it is the exception rather than the rule.
    let sync = parse(&["ocx-mirror", "registry", "sync", "corp/registry.yml"]);

    assert_eq!(sync.spec, std::path::Path::new("corp/registry.yml"));
}

#[test]
fn every_flag_is_off_by_default() {
    let sync = parse(&["ocx-mirror", "registry", "sync"]);

    assert!(!sync.options.dry_run);
    assert!(!sync.options.fail_fast);
    assert!(!sync.options.repair_catalog);
    assert_eq!(sync.options.cache_dir, None);
    assert_eq!(
        sync.options.format,
        crate::command::package::options::OutputFormat::Plain
    );
}

#[test]
fn the_flags_parse_alongside_a_spec_path() {
    let sync = parse(&[
        "ocx-mirror",
        "registry",
        "sync",
        "corp/registry.yml",
        "--dry-run",
        "--fail-fast",
        "--repair-catalog",
        "--cache-dir",
        "/var/cache/ocx-mirror",
        "--format",
        "json",
    ]);

    assert_eq!(sync.spec, std::path::Path::new("corp/registry.yml"));
    assert!(sync.options.dry_run);
    assert!(sync.options.fail_fast);
    assert!(sync.options.repair_catalog);
    assert_eq!(
        sync.options.cache_dir.as_deref(),
        Some(std::path::Path::new("/var/cache/ocx-mirror"))
    );
    assert_eq!(
        sync.options.format,
        crate::command::package::options::OutputFormat::Json
    );
}

// ── C-045's exit-code decision ───────────────────────────────────────────────

use crate::error::MirrorError;
use crate::pipeline::registry_sync::report::{
    PackageOutcome, PackageReport, RegistrySyncReport, RunCounters, SignatureCounts, SourceReport,
};

fn report(outcomes: &[(&str, PackageOutcome, Option<&str>)]) -> RegistrySyncReport {
    let failed = outcomes
        .iter()
        .filter(|(_, outcome, _)| *outcome == PackageOutcome::Failed)
        .count();
    RegistrySyncReport {
        sources: vec![SourceReport {
            as_name: "ocx.sh".to_string(),
            short_circuited: false,
            packages: outcomes
                .iter()
                .map(|(name, outcome, detail)| PackageReport {
                    name: (*name).to_string(),
                    outcome: *outcome,
                    detail: detail.map(str::to_string),
                    signatures: SignatureCounts::default(),
                })
                .collect(),
        }],
        counters: RunCounters {
            total: outcomes.len(),
            copied: outcomes.len() - failed,
            skipped: 0,
            failed,
        },
        estimated_bytes: None,
    }
}

#[test]
fn a_clean_run_is_not_classified_as_a_failure() {
    let clean = report(&[
        ("kitware/cmake", PackageOutcome::Copied, None),
        ("ninja-build/ninja", PackageOutcome::Skipped, None),
    ]);

    assert_eq!(super::failed_packages(&clean), None);
}

#[test]
fn a_run_with_per_package_failures_names_each_one_and_exits_one() {
    // C-040's aggregating class: the run finished, some packages did not, and
    // the operator needs to know which — not merely that some did.
    let mixed = report(&[
        ("kitware/cmake", PackageOutcome::Copied, None),
        ("broken/one", PackageOutcome::Failed, Some("manifest 404")),
        ("broken/two", PackageOutcome::Failed, None),
    ]);

    let errors = super::failed_packages(&mixed).expect("two packages failed");

    assert_eq!(
        errors,
        vec![
            "ocx.sh/broken/one: manifest 404".to_string(),
            "ocx.sh/broken/two: failed".to_string(),
        ]
    );
    assert_eq!(
        MirrorError::ExecutionFailed(errors).kind_exit_code(),
        ocx_lib::cli::ExitCode::Failure
    );
}

#[test]
fn a_counted_failure_with_no_package_row_still_carries_a_message() {
    // The counter decides the exit code, the rows decide the wording, and they
    // can disagree. `ExecutionFailed(vec![])` would exit 1 printing the header
    // "mirror execution failed:" with nothing under it.
    let mut counted = report(&[("kitware/cmake", PackageOutcome::Copied, None)]);
    counted.counters.failed = 1;

    let errors = super::failed_packages(&counted).expect("the counter says the run failed");

    assert_eq!(errors, vec![counted.summary_line()]);
}

#[test]
fn an_empty_run_is_not_a_failure() {
    // A spec whose filters select nothing still exits 0 — "nothing to do" is
    // not "something went wrong".
    assert_eq!(super::failed_packages(&RegistrySyncReport::default()), None);
}

/// A path under a directory that exists, naming a file that does not.
const MISSING_SPEC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/no-such-registry.yml");

#[tokio::test]
async fn a_whole_run_abort_keeps_its_own_exit_code() {
    // The other half of the split: an abort propagates verbatim instead of
    // collapsing into `ExecutionFailed` (1). An absent spec aborts inside
    // `load_registry_spec`, so this drives the real `execute` without a source
    // or a destination — and without reaching WP-14's orchestrator.
    let sync = parse(&["ocx-mirror", "registry", "sync", MISSING_SPEC]);
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));

    let error = sync.execute(&printer).await.expect_err("the spec does not exist");

    assert!(matches!(error, MirrorError::SpecNotFound(_)), "got {error:?}");
    assert_eq!(error.kind_exit_code(), ocx_lib::cli::ExitCode::NotFound);
}

#[test]
fn an_unknown_flag_is_a_usage_error() {
    // The negative half: `try_parse_from` succeeding on everything would make
    // the assertions above vacuous.
    assert!(
        Harness::try_parse_from(["ocx-mirror", "registry", "sync", "--no-such-flag"]).is_err(),
        "an unknown flag must not parse"
    );
}

#[test]
fn a_second_positional_is_a_usage_error() {
    // One spec, not a list — the asymmetry with `package sync` is deliberate.
    assert!(
        Harness::try_parse_from(["ocx-mirror", "registry", "sync", "one.yml", "two.yml"]).is_err(),
        "registry sync takes exactly one spec"
    );
}
