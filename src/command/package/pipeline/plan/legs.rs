// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `plan.json`'s per-platform CI legs — the spec's test matrix, resolved.
//!
//! A renderer for a forge the GitHub templates do not cover reads these
//! instead of re-deriving the matrix from `mirror.yml`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::spec::{self, MirrorSpec};

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

/// The spec's test matrix, resolved into the plan document.
///
/// Deliberately the same derivations `build_matrix` uses — `resolved_id`,
/// `resolved_shell`, `native_shell`, `platform_key_slug`,
/// `platform_without_features`, `infer_libc_from_image`. A second, drifting
/// copy would hand a non-GitHub renderer JUnit names `pipeline push` never
/// looks up, and push fails closed on a missing result. Pinned to
/// `build_matrix` by `plan_legs_match_the_rendered_matrix`.
///
/// Unlike `build_matrix` this drops nothing: the plan carries what the spec
/// says, and `validate` is the gate an invalid spec never gets past.
pub fn build_legs(spec: &MirrorSpec) -> BTreeMap<String, PlanLeg> {
    let Some(platforms) = &spec.platforms else {
        return BTreeMap::new();
    };
    let top_level_tests = spec.tests.clone().unwrap_or_default();

    platforms
        .iter()
        .map(|(key, config)| {
            let containers = match config.containers.as_deref().filter(|c| !c.is_empty()) {
                Some(containers) => containers
                    .iter()
                    .map(|container| PlanContainer {
                        id: container.resolved_id(),
                        image: Some(container.image.clone()),
                        shell: container.resolved_shell(),
                        libc: Some(spec::infer_libc_from_image(&container.image).to_owned()),
                        setup: container.setup.clone().filter(|setup| !setup.is_empty()),
                    })
                    .collect(),
                // The same sentinel the gating keys a container-less platform's
                // verdict on.
                None => vec![PlanContainer {
                    id: "_native_".to_owned(),
                    image: None,
                    shell: config.native_shell(key).to_owned(),
                    libc: None,
                    setup: None,
                }],
            };

            (
                key.clone(),
                PlanLeg {
                    runner: config.runner.clone(),
                    platform_slug: spec::platform_key_slug(key),
                    docker_platform: spec::platform_without_features(key),
                    containers,
                    // A per-platform `tests:` REPLACES the top-level list — no
                    // merge. A flat top-level `tests` in the plan would be
                    // wrong for any spec using the override, which is why this
                    // is keyed by platform.
                    tests: config.tests.clone().unwrap_or_else(|| top_level_tests.clone()),
                },
            )
        })
        .collect()
}
