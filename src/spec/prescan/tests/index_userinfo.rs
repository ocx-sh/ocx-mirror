// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-005 job 3 — userinfo embedded in a source's `index:` URL.

use std::path::Path;

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;

/// Distinctive enough that finding it in the output is proof of a leak.
const SENTINEL: &str = "SUPERSECRET-DO-NOT-LEAK";

#[test]
fn an_index_url_carrying_userinfo_is_a_usage_error() {
    let error = rejection("kind: registry\nsources:\n  - index: https://user:pass@index.example/\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
}

/// The never-echo guarantee again, on the one job whose offending value *is* a
/// URL: naming the URL would reproduce the embedded password verbatim.
#[test]
fn the_rejection_names_the_field_and_never_the_url() {
    let yaml = format!("kind: registry\nsources:\n  - index: https://operator:{SENTINEL}@index.example/\n");
    let error = rejection(&yaml);
    let rendered = error.to_string();

    assert!(
        !rendered.contains(SENTINEL),
        "the embedded password leaked into the message: {rendered}"
    );
    assert!(
        !rendered.contains("index.example"),
        "the raw URL leaked into the message: {rendered}"
    );
    assert!(
        rendered.contains("sources[0].index"),
        "the message must name the field: {rendered}"
    );
    assert!(
        rendered.contains("OCX_AUTH_<slug>_TOKEN"),
        "the message must name the environment variable to use instead: {rendered}"
    );
}

#[test]
fn a_username_only_url_is_still_a_usage_error() {
    let error = rejection("kind: registry\nsources:\n  - index: https://operator@index.example/\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
}

#[test]
fn the_offending_source_is_reported_by_index() {
    let yaml = concat!(
        "kind: registry\n",
        "sources:\n",
        "  - index: https://clean.example/\n",
        "  - index: https://user:pass@dirty.example/\n",
    );
    let error = rejection(yaml);

    assert!(
        error.to_string().contains("sources[1].index"),
        "the message must index the offending source: {error}"
    );
}

#[test]
fn a_credential_free_index_url_is_accepted() {
    let yaml = "kind: registry\nsources:\n  - index: https://index.ocx.sh/\n";

    assert!(pre_scan(&merged(yaml), Path::new(SPEC_PATH), REGISTRY_KIND).is_ok());
}
