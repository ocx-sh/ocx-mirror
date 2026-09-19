// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The pin that keeps the rendered matrix and `plan.json`'s `legs`
//! describing the same legs.

use super::support::*;
use crate::command::package::pipeline::plan::build_legs;

/// One leg, in the shape both derivations can be read into.
type Leg = (String, String, Vec<String>, String, String, String, Vec<String>);

/// Read a fixture's YAML so the same bytes drive the renderer and `build_legs`.
fn fixture_yaml(fixture: &str) -> String {
    let path = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/")).join(fixture);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {} must be readable: {e}", path.display()))
}

/// Flatten the rendered workflow's `matrix.include` into [`Leg`] tuples.
fn rendered_legs(fixture: &str) -> Vec<Leg> {
    let workflow = workflow_for(fixture);
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&workflow).expect("the workflow must be valid YAML");
    parsed["jobs"]["test"]["strategy"]["matrix"]["include"]
        .as_sequence()
        .unwrap_or_else(|| panic!("no test matrix in:\n{workflow}"))
        .iter()
        .map(|entry| {
            let text = |key: &str| entry[key].as_str().unwrap_or_default().to_owned();
            // A single label renders as a scalar, a set as a flow sequence.
            let runner = match &entry["runner"] {
                serde_yaml_ng::Value::Sequence(labels) => labels
                    .iter()
                    .map(|label| label.as_str().expect("runner labels are strings").to_owned())
                    .collect(),
                other => vec![other.as_str().expect("a scalar runner is a string").to_owned()],
            };
            let tests = entry["tests"]
                .as_sequence()
                .expect("every leg carries its tests")
                .iter()
                .map(|test| test["name"].as_str().expect("a test has a name").to_owned())
                .collect();
            (
                text("platform"),
                text("platform_slug"),
                runner,
                text("container_id"),
                text("shell"),
                text("container_image"),
                tests,
            )
        })
        .collect()
}

/// Flatten `build_legs` into the same [`Leg`] tuples.
fn plan_legs(fixture: &str) -> Vec<Leg> {
    let spec = spec_from_yaml(&fixture_yaml(fixture));
    build_legs(&spec)
        .into_iter()
        .flat_map(|(key, leg)| {
            let names: Vec<String> = leg.tests.iter().map(|test| test.name.clone()).collect();
            leg.containers.into_iter().map(move |container| {
                (
                    key.clone(),
                    leg.platform_slug.clone(),
                    leg.runner.clone(),
                    container.id,
                    container.shell,
                    container.image.unwrap_or_default(),
                    names.clone(),
                )
            })
        })
        .collect()
}

#[test]
fn plan_legs_match_the_rendered_matrix() {
    // The anti-drift pin. `pipeline push` keys its verdict on
    // `(version, platform_slug, container id, test name)` and fails closed on a
    // missing result, so a `plan.json` leg naming a container the rendered
    // matrix never produced sends a non-GitHub renderer to write JUnit files
    // push will not look up. One fixture exercising every branch: container
    // legs with an inferred shell, a registry-qualified image, and a native leg
    // in the same spec.
    for fixture in [
        "mirror-container-mixed.yml",
        "mirror-runner-labels.yml",
        "mirror-full-platforms.yml",
    ] {
        assert_eq!(
            plan_legs(fixture),
            rendered_legs(fixture),
            "{fixture}: `build_legs` and `build_matrix` describe different legs"
        );
    }
}
