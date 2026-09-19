// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `plan.json`'s per-platform CI legs — the spec's test matrix, resolved.
//!
//! The types only; the derivation from a [`crate::spec::MirrorSpec`] lands
//! with issue #77. A renderer for a forge the GitHub templates do not cover
//! reads these instead of re-deriving the matrix from `mirror.yml`.

use serde::{Deserialize, Serialize};

/// One platform's CI leg: where it runs, what it runs in, what it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
pub struct PlanLeg {
    /// Runner label set — the job runs on a runner carrying *all* of these
    /// (GitHub `runs-on`, GitLab `tags`). Always a list, even for the
    /// single-label spelling.
    pub runner: Vec<String>,
    /// `spec::platform_key_slug(key)` — the bundle/JUnit join key.
    pub platform_slug: String,
    /// The platform key with any `+libc.*` suffix stripped — what
    /// `docker run --platform` and `ocx package test --platform` accept.
    pub docker_platform: String,
    /// One entry per container image, or the single `_native_` sentinel
    /// when the platform declares no `containers:`.
    pub containers: Vec<PlanContainer>,
    /// The effective test list for this platform: `platforms.<k>.tests`
    /// when set, else the top-level `tests:`. Never a merge of the two.
    pub tests: Vec<crate::spec::TestEntry>,
}

/// One container leg of a platform, or the native sentinel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
pub struct PlanContainer {
    /// JUnit / gating join key. `pipeline push` looks results up by it, so a
    /// renderer that invents its own finds no result and the gate fails closed.
    pub id: String,
    /// Absent on the `_native_` leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub shell: String,
    /// `gnu` | `musl`. Absent on the `_native_` leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
    /// Provisioning commands, one per Dockerfile `RUN`. Absent when the
    /// container declares none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Vec<String>>,
}
