// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-053 — the pre-scan over `mirror.yml`, where `expected_kind` is `None`.
//!
//! Wiring an existing refusal onto a *new* surface is the risk this module
//! exists for: every `mirror.yml` in the wild now goes through the credential
//! deny-list, and none of them declares a `kind:`. So the two halves are
//! asserted together — the deny-list still fires at depth, and the `kind:`
//! job stays off for this caller while remaining on for the callers that have
//! one.

use std::path::{Path, PathBuf};

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;

/// A value distinctive enough that finding it in a message is proof of a leak.
const SENTINEL: &str = "hunter2-canary";

/// The `mirror.yml` path a message is expected to name.
const MIRROR_PATH: &str = "mirror.yml";

/// A `mirror.yml` as `load_spec` hands it to the pre-scan: no `kind:`, and a
/// `sign:` block, because that block's field names are the ones the ADR chose
/// to sit *outside* the deny-list.
const MIRROR_SPEC: &str = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
platforms:
  linux/amd64:
    runner: ubuntu-latest
sign:
  key:
    ref: file:///run/secrets/mirror.key
    passphrase: env://MIRROR_KEY_PASSPHRASE
"#;

fn scan(yaml: &str) -> Result<(), MirrorError> {
    pre_scan(&merged(yaml), Path::new(MIRROR_PATH), None)
}

fn mirror_rejection(yaml: &str) -> MirrorError {
    scan(yaml).expect_err("the document must be rejected by the pre-scan")
}

/// The refusal reaches a `mirror.yml` at depth, names the dotted path and the
/// remedy, and never the value — a `mirror.yml` is committed to a public
/// repository, so the message is the one that ends up in the log.
#[test]
fn a_credential_at_depth_is_refused_with_its_dotted_path_and_the_remedy() {
    let yaml = format!("name: shfmt\nplatforms:\n  linux/amd64:\n    runner: ubuntu-latest\n    token: {SENTINEL}\n");
    let error = mirror_rejection(&yaml);
    let rendered = error.to_string();

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        rendered.contains("platforms.linux/amd64.token"),
        "the message must name the dotted path: {rendered}"
    );
    assert!(
        rendered.contains("OCX_AUTH_<slug>_TOKEN"),
        "the message must name the remedy: {rendered}"
    );
    assert!(
        !rendered.contains(SENTINEL),
        "the credential leaked into the message: {rendered}"
    );
}

/// The whole point of the `Option<&str>` signature: a `mirror.yml` carries no
/// `kind:` field, so demanding one would reject every spec that exists today.
#[test]
fn a_mirror_spec_is_not_refused_for_lacking_a_kind() {
    assert!(
        scan(MIRROR_SPEC).is_ok(),
        "a `mirror.yml` must pass the pre-scan without a `kind:`"
    );
}

/// ...and the demand survives for the callers that do have one, so the
/// signature change relaxed the check for exactly one caller and not for all.
#[test]
fn the_kind_demand_still_applies_to_a_registry_or_dist_spec() {
    for kind in [REGISTRY_KIND, DIST_KIND] {
        let error = pre_scan(&merged(MIRROR_SPEC), Path::new(MIRROR_PATH), Some(kind))
            .expect_err("a document without `kind:` must be refused when one is expected");
        assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
        assert!(
            error.to_string().contains(KIND_KEY),
            "the message must name `kind` for {kind}: {error}"
        );
    }
}

/// The `sign:` block's own secret-class fields are named `passphrase` and
/// `identity_token` deliberately: both sit outside the deny-list, so a spec
/// configuring signing is not refused by the guard that shipped with it.
#[test]
fn a_sign_block_does_not_collide_with_the_credential_deny_list() {
    let yaml = "sign:\n  keyless:\n    identity_token: file:///run/token\n";
    assert!(scan(yaml).is_ok(), "`identity_token` must not read as a credential key");
    assert!(
        scan("sign:\n  key:\n    ref: env://K\n    passphrase: env://P\n").is_ok(),
        "`passphrase` must not read as a credential key"
    );
}

/// The C-053 gate. A new refusal on an existing surface is only safe if no
/// document already in the tree trips it, and the corpus is the population
/// that answers that — a spec that must fail *validation* still has to reach
/// the validator rather than dying in the pre-scan.
#[test]
fn every_shipped_yaml_fixture_survives_the_credential_pre_scan() {
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"));
    let mut checked = 0;

    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("fixture directory is readable") {
            let path = entry.expect("readable directory entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "yml") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("fixture is readable");
            let Ok(document) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&source) else {
                // A fixture that is not YAML at all is some other test's
                // business; the pre-scan never sees it.
                continue;
            };
            assert!(
                pre_scan(&document, &path, None).is_ok(),
                "{} is refused by the credential pre-scan",
                path.display()
            );
            checked += 1;
        }
    }

    assert!(checked > 0, "no fixtures found under {}", root.display());
}
