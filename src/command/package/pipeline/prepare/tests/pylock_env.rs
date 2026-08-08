// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── W2.3: pylock env task building (network-free — interpreter dep injected) ──

fn pylock_fixture_spec_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pylock.yml"))
}

#[test]
fn interpreter_pin_selects_the_matching_libc_leaf_per_leg() {
    // D5 leaf pinning must discriminate by the wheels key's `+libc.*`
    // feature: the `Any`-candidate fake used elsewhere would keep every
    // other test green even if the libc threading broke — this one reds.
    fn candidate(digest_byte: char, platform: &str) -> (ocx_lib::oci::Identifier, ocx_lib::oci::Platform) {
        let reference = format!("ocx.sh/cpython:3.13@sha256:{}", digest_byte.to_string().repeat(64));
        (
            ocx_lib::oci::Identifier::parse(&reference).expect("candidate reference parses"),
            platform.parse().expect("candidate platform parses"),
        )
    }
    let candidates = vec![
        candidate('a', "linux/amd64+libc.glibc"),
        candidate('b', "linux/amd64+libc.musl"),
    ];

    let musl = select_interpreter_pin(
        "ocx.sh/cpython:3.13",
        &candidates,
        &"linux/amd64+libc.musl".parse().unwrap(),
    )
    .expect("musl leg resolves a pin");
    assert!(
        musl.identifier.to_string().contains(&"b".repeat(64)),
        "musl leg must pin the musl leaf, got {}",
        musl.identifier
    );

    let glibc = select_interpreter_pin(
        "ocx.sh/cpython:3.13",
        &candidates,
        &"linux/amd64+libc.glibc".parse().unwrap(),
    )
    .expect("glibc leg resolves a pin");
    assert!(
        glibc.identifier.to_string().contains(&"a".repeat(64)),
        "glibc leg must pin the glibc leaf, got {}",
        glibc.identifier
    );

    // A featureless leg matches neither libc-featured offer (an offer's
    // features must be a subset of the requirement's) — hard error naming
    // the platform, not a silent arbitrary pick.
    let error = select_interpreter_pin("ocx.sh/cpython:3.13", &candidates, &"linux/amd64".parse().unwrap())
        .expect_err("featureless leg has no compatible leaf");
    assert!(format!("{error}").contains("no entry compatible"), "got: {error}");
}

#[tokio::test]
async fn build_env_tasks_selects_wheels_per_applicable_platform() {
    let spec_path = pylock_fixture_spec_path();
    let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
    let spec_dir = spec_path.parent().unwrap();

    let candidates = fake_interpreter_candidates();
    let tasks = build_env_tasks(&spec, spec_dir, "1.0.0", &candidates, None)
        .await
        .expect("build_env_tasks succeeds");

    // Two declared wheels keys → two env legs.
    assert_eq!(tasks.len(), 2, "2 wheels keys → 2 env legs");
    let mut platforms: Vec<String> = tasks.iter().map(|task| task.platform.to_string()).collect();
    platforms.sort();
    assert_eq!(platforms, vec!["linux/amd64".to_string(), "linux/arm64".to_string()]);

    for task in &tasks {
        assert_eq!(task.normalized_version, "1.0.0");
        assert_eq!(task.source_version, "1.0.0");
        // Both fixture wheels are `none-any` → both apply on every platform.
        assert_eq!(task.wheels.len(), 2, "2 wheels per env leg");
        let names: Vec<&str> = task.wheels.iter().map(|wheel| wheel.filename.as_str()).collect();
        assert!(names.iter().any(|name| name.starts_with("pycowsay-")), "{names:?}");
        assert!(names.iter().any(|name| name.starts_with("six-")), "{names:?}");
        for wheel in &task.wheels {
            assert!(
                wheel.wheel_repository.starts_with("pip-packages/"),
                "repo-relative wheel repository: {}",
                wheel.wheel_repository
            );
            assert_eq!(wheel.url.scheme(), "https");
        }
        assert!(
            task.interpreter.identifier.to_string().contains("cpython"),
            "the injected interpreter dependency is threaded onto every task"
        );
    }
}

#[tokio::test]
async fn build_env_tasks_is_empty_for_unknown_version() {
    let spec_path = pylock_fixture_spec_path();
    let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
    let spec_dir = spec_path.parent().unwrap();

    let candidates = fake_interpreter_candidates();
    let tasks = build_env_tasks(&spec, spec_dir, "9.9.9", &candidates, None)
        .await
        .expect("build_env_tasks succeeds");
    assert!(tasks.is_empty(), "no bare env tag matches an unknown version");
}

#[tokio::test]
async fn build_env_tasks_restricts_to_plan_platforms() {
    // Backfill-partial: the plan lists only the outstanding platform
    // (linux/arm64), so prepare must compose that one alone — not the
    // already-published linux/amd64 the spec also declares.
    let spec_path = pylock_fixture_spec_path();
    let spec = spec::load_spec(&spec_path).await.expect("fixture spec loads");
    let spec_dir = spec_path.parent().unwrap();

    let allowed: std::collections::HashSet<String> = ["linux/arm64".to_string()].into_iter().collect();
    let candidates = fake_interpreter_candidates();
    let tasks = build_env_tasks(&spec, spec_dir, "1.0.0", &candidates, Some(&allowed))
        .await
        .expect("build_env_tasks succeeds");

    assert_eq!(tasks.len(), 1, "plan restricts to the single outstanding platform");
    assert_eq!(tasks[0].platform.to_string(), "linux/arm64");
}
