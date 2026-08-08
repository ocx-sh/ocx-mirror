// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `plan` test module.

use super::super::*;

/// Drive one async body to completion under the current thread's runtime.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Test helper: entry with the v2 fields defaulted so schema-shape tests
/// stay focused on the field under assertion.
pub fn entry(version: &str, platforms: &[&str], kind: PlanVersionKind) -> PlanVersionEntry {
    PlanVersionEntry {
        version: version.to_string(),
        platforms: platforms.iter().map(|p| p.to_string()).collect(),
        kind,
        source_version: version.to_string(),
        variant: None,
        assets: vec![],
        pylock: None,
    }
}

pub fn metadata(json: &str) -> Metadata {
    serde_json::from_str(json).expect("metadata fixture parses")
}
