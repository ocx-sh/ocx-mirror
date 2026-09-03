// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-005 job 2 — the `kind:` discriminator.
//!
//! This job is why the pre-scan exists at all: `RegistrySpec` carries
//! `deny_unknown_fields` and no `kind` field, so the same check after
//! deserialization is unreachable in exactly the case it is for.

use std::path::Path;

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;

#[test]
fn an_absent_kind_is_a_usage_error() {
    let error = rejection("target:\n  registry: corp.example.com\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        error.to_string().contains("kind"),
        "the message must name kind: {error}"
    );
}

#[test]
fn a_non_string_kind_is_a_usage_error() {
    let error = rejection("kind: 3\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
}

#[test]
fn a_foreign_kind_is_a_usage_error() {
    let error = rejection("kind: package\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
}

/// A misplaced `mirror.yml` is the case this job exists for: it must be
/// refused for its `kind`, not for whichever field serde happens to trip over
/// first.
#[test]
fn a_mirror_spec_handed_to_registry_sync_is_refused_for_its_kind() {
    let mirror_yaml = "name: cmake\ntarget:\n  registry: ghcr.io\n  repository: ocx-contrib/kitware/cmake\n";
    let error = rejection(mirror_yaml);

    assert!(
        error.to_string().contains("kind"),
        "a mirror spec must be refused for its missing kind, not a field shape: {error}"
    );
}

#[test]
fn the_registry_kind_is_accepted() {
    assert!(pre_scan(&merged("kind: registry\n"), Path::new(SPEC_PATH), Some(REGISTRY_KIND)).is_ok());
}

/// The discriminator is per-spec-type: the same document that satisfies
/// `registry sync` is refused by `dist sync`, which is the whole point of
/// parameterising the check rather than hardcoding one kind.
#[test]
fn a_registry_document_is_refused_as_a_dist_spec() {
    let error = pre_scan(&merged("kind: registry\n"), Path::new(SPEC_PATH), Some(DIST_KIND))
        .expect_err("a registry document must not load as a dist spec");

    assert!(
        error.to_string().contains("dist"),
        "the rejection must name the kind that was expected: {error}"
    );
}
