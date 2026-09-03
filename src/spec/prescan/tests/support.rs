// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Shared fixtures for the `prescan` test corpus.

use std::path::Path;

use serde_yaml_ng::Value;

use crate::error::MirrorError;

/// The spec path every message is expected to name.
pub const SPEC_PATH: &str = "registry.yml";

/// Parse a test document the way the loader hands one to the pre-scan: as a
/// merged `serde_yaml_ng::Value`, never a typed spec.
pub fn merged(yaml: &str) -> Value {
    serde_yaml_ng::from_str(yaml).expect("test document must parse as YAML")
}

/// Run the pre-scan over `yaml` and return the rejection message.
///
/// Panics if the document is accepted — a test that expected a rejection and
/// got silence has found the bug, and should say so at the assertion, not by
/// unwrapping nothing.
pub fn rejection(yaml: &str) -> MirrorError {
    super::super::pre_scan(&merged(yaml), Path::new(SPEC_PATH), Some(super::super::REGISTRY_KIND))
        .expect_err("document must be rejected by the pre-scan")
}

/// A minimal, valid registry spec — every field a real one carries, so a test
/// asserting acceptance also pins that no legitimate key collides with the
/// credential deny-list.
pub const VALID_SPEC: &str = r#"
kind: registry
target:
  registry: corp.example.com
  repository: mirror
output: ./public
destination: "{registry}/{namespace}/{package}"
on_error: continue
concurrency:
  max_blobs: 4
  max_retries: 3
sources:
  - registry: ghcr.io
    index: https://index.ocx.sh
    as: ocx.sh
    include: ["kitware/*"]
    exclude: []
    trusted_hosts: []
"#;
