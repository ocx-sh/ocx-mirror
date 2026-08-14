// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-005 job 1 — the credential deny-list, at any depth.

use std::path::Path;

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;

/// A value distinctive enough that finding it anywhere in the output is proof
/// of a leak, not a coincidence.
const SENTINEL: &str = "SUPERSECRET-DO-NOT-LEAK";

#[test]
fn a_credential_key_is_refused_as_a_usage_error() {
    let error = rejection("kind: registry\npassword: hunter2\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        error.to_string().contains("password"),
        "the message must name the offending key: {error}"
    );
}

/// **The security assertion.** Naming the key while echoing the value would
/// put the secret into every log the run writes, which is the failure this
/// guarantee exists to prevent — and a test that only checks the key name
/// appears passes while the value leaks.
#[test]
fn the_rejection_names_the_key_and_never_the_secret_value() {
    let error = rejection(&format!("kind: registry\nsources:\n  - token: {SENTINEL}\n"));
    let rendered = error.to_string();

    assert!(
        !rendered.contains(SENTINEL),
        "the secret value leaked into the message: {rendered}"
    );
    assert!(
        rendered.contains("sources[0].token"),
        "the message must name the key path: {rendered}"
    );
    assert!(
        rendered.contains("OCX_AUTH_<slug>_TOKEN"),
        "the message must name the environment variable to use instead: {rendered}"
    );
}

#[test]
fn a_credential_nested_at_depth_reports_its_dotted_path() {
    let error = rejection("kind: registry\ntarget:\n  registry: corp.example.com\n  auth: whatever\n");

    assert!(
        error.to_string().contains("target.auth"),
        "the message must name the full path: {error}"
    );
}

#[test]
fn a_credential_inside_a_sequence_reports_its_index() {
    let yaml = "kind: registry\nsources:\n  - index: https://a.example\n  - index: https://b.example\n    secret: x\n";
    let error = rejection(yaml);

    assert!(
        error.to_string().contains("sources[1].secret"),
        "the message must index the sequence element: {error}"
    );
}

#[test]
fn key_comparison_is_case_insensitive() {
    let error = rejection("kind: registry\nAPI_Key: x\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
}

#[test]
fn a_credential_key_is_refused_whatever_its_value_type() {
    // A null value and a nested mapping are both refused: the deny-list reads
    // the key, never the value, so there is no value shape that exempts one.
    for yaml in [
        "kind: registry\ncredentials:\n",
        "kind: registry\ncredentials:\n  inner: x\n",
    ] {
        let error = rejection(yaml);
        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "not refused: {yaml}");
    }
}

#[test]
fn every_deny_listed_key_is_refused() {
    for key in CREDENTIAL_DENY_LIST {
        let error = rejection(&format!("kind: registry\n{key}: x\n"));
        assert!(
            error.to_string().contains(*key),
            "deny-listed key {key} was not named: {error}"
        );
    }
}

/// Pins the deny-list's documented limit so widening it is a deliberate act
/// with a failing test attached, not a silent drive-by. Catching this shape
/// means reading values to guess at them, which is the one thing this function
/// must not do.
#[test]
fn a_credential_under_an_innocuous_key_name_is_a_known_limit() {
    let yaml = format!("kind: registry\nregistry_auth:\n  name: token\n  value: {SENTINEL}\n");

    assert!(pre_scan(&merged(&yaml), Path::new(SPEC_PATH)).is_ok());
}

#[test]
fn a_valid_registry_spec_carries_no_credential_shaped_key() {
    // Also pins the inverse: no legitimate `registry.yml` key collides with
    // the deny-list, so the guard costs operators nothing.
    assert!(pre_scan(&merged(VALID_SPEC), Path::new(SPEC_PATH)).is_ok());
}
