// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The root `--format` / `--json` folded into each command's own format.

use clap::Parser;

use super::*;
use crate::pipeline::options::OutputFormat::{Json, Plain};

/// The binary's root shape, minus the flags `apply_format` never reads.
#[derive(Parser)]
struct Root {
    #[command(flatten)]
    format: ocx_console::Format,
    #[command(subcommand)]
    command: Command,
}

fn parse(args: &[&str]) -> Command {
    let mut argv = vec!["ocx-mirror"];
    argv.extend_from_slice(args);
    let Root { format, mut command } = Root::try_parse_from(argv).expect("parse");
    command.apply_format(format.requested());
    command
}

fn sync_format(command: Command) -> OutputFormat {
    match command {
        Command::Package(package::PackageCommand::Sync(cmd)) => cmd.options.format,
        Command::Package(package::PackageCommand::Check(cmd)) => cmd.options.format,
        _ => panic!("not package sync or check"),
    }
}

fn plan_format(command: Command) -> Option<OutputFormat> {
    match command {
        Command::Package(package::PackageCommand::Pipeline(package::pipeline::PipelineCommand::Plan(cmd))) => {
            cmd.format
        }
        _ => panic!("not package pipeline plan"),
    }
}

/// Without a root flag every command keeps exactly what it parsed.
#[test]
fn no_root_flag_changes_nothing() {
    assert_eq!(sync_format(parse(&["package", "sync", "m.yml"])), Plain);
    assert_eq!(
        sync_format(parse(&["package", "sync", "m.yml", "--format", "json"])),
        Json
    );
    assert_eq!(plan_format(parse(&["package", "pipeline", "plan"])), None);
    assert_eq!(
        plan_format(parse(&["package", "pipeline", "plan", "--format", "plain"])),
        Some(Plain)
    );
}

/// Both root spellings reach a defaulted per-command flag; JSON wins when
/// either side asks for it.
#[test]
fn root_json_reaches_a_defaulted_command_format() {
    assert_eq!(sync_format(parse(&["--json", "package", "sync", "m.yml"])), Json);
    assert_eq!(
        sync_format(parse(&["--format", "json", "package", "check", "m.yml"])),
        Json
    );
    assert_eq!(
        sync_format(parse(&["--json", "package", "sync", "m.yml", "--format", "plain"])),
        Json
    );
    assert_eq!(
        sync_format(parse(&[
            "--format", "plain", "package", "sync", "m.yml", "--format", "json"
        ])),
        Json
    );
}

/// `plan` defaults to JSON under GitHub Actions; an explicit root `plain`
/// switches that off, and JSON from either side still wins.
#[test]
fn root_format_decides_plan_when_plan_has_none() {
    assert_eq!(
        plan_format(parse(&["--format", "plain", "package", "pipeline", "plan"])),
        Some(Plain)
    );
    assert_eq!(
        plan_format(parse(&["--json", "package", "pipeline", "plan"])),
        Some(Json)
    );
    assert_eq!(
        plan_format(parse(&["--json", "package", "pipeline", "plan", "--format", "plain"])),
        Some(Json)
    );
    assert_eq!(
        plan_format(parse(&[
            "--format", "plain", "package", "pipeline", "plan", "--format", "json"
        ])),
        Some(Json)
    );
}

#[test]
fn root_json_reaches_registry_dist_sign_and_version() {
    match parse(&["--json", "registry", "sync"]) {
        Command::Registry(registry::RegistryCommand::Sync(cmd)) => assert_eq!(cmd.options.format, Json),
        _ => panic!("not registry sync"),
    }
    match parse(&["--json", "dist", "sync"]) {
        Command::Dist(dist::DistCommand::Sync(cmd)) => assert_eq!(cmd.options.format, Json),
        _ => panic!("not dist sync"),
    }
    match parse(&["--json", "package", "pipeline", "sign"]) {
        Command::Package(package::PackageCommand::Pipeline(package::pipeline::PipelineCommand::Sign(cmd))) => {
            assert_eq!(cmd.format, Json);
        }
        _ => panic!("not package pipeline sign"),
    }
    match parse(&["--json", "version"]) {
        Command::Version(cmd) => assert_eq!(cmd.format, Some(Json)),
        _ => panic!("not version"),
    }
    match parse(&["version"]) {
        Command::Version(cmd) => assert_eq!(cmd.format, None),
        _ => panic!("not version"),
    }
}

/// Output format is a root option, as in ocx — `version` has no flag of its own.
#[test]
fn version_takes_no_format_flag() {
    assert!(Root::try_parse_from(["ocx-mirror", "version", "--json"]).is_err());
    assert!(Root::try_parse_from(["ocx-mirror", "version", "--format", "json"]).is_err());
}
