// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use crate::error::MirrorError;

// -- extends tests --

#[tokio::test]
async fn load_spec_without_extends() {
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("mirror.yml");
    std::fs::write(
        &spec_path,
        r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#,
    )
    .unwrap();

    let spec = load_spec(&spec_path).await.unwrap();
    assert_eq!(spec.name, "test");
    assert!(spec.cascade.enabled);
}

#[tokio::test]
async fn load_spec_extends_happy_path() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(
        dir.path().join("base.yml"),
        r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
cascade: true
build_timestamp: none
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("child.yml"),
        r#"
extends: base.yml
name: child-test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#,
    )
    .unwrap();

    let spec = load_spec(&dir.path().join("child.yml")).await.unwrap();
    assert_eq!(spec.name, "child-test");
    assert_eq!(spec.target.registry, "ocx.sh");
    assert!(spec.cascade.enabled);
    assert_eq!(spec.build_timestamp, BuildTimestampFormat::None);
}

#[tokio::test]
async fn load_spec_extends_shallow_override() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(
        dir.path().join("base.yml"),
        r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "base\\.tar\\.gz"
  darwin/arm64:
    - "base-darwin\\.tar\\.gz"
versions:
  min: "1.0.0"
  new_per_run: 5
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("child.yml"),
        r#"
extends: base.yml
name: child
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
versions:
  min: "8.0.0"
  new_per_run: 10
"#,
    )
    .unwrap();

    let spec = load_spec(&dir.path().join("child.yml")).await.unwrap();
    // versions should be entirely replaced, not deep-merged
    let versions = spec.versions.unwrap();
    assert_eq!(versions.min.as_deref(), Some("8.0.0"));
    assert_eq!(versions.new_per_run, Some(10));
    // assets should still come from base (not overridden)
    assert!(matches!(spec.source, Source::GithubRelease { .. }));
}

#[tokio::test]
async fn load_spec_extends_circular() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(
        dir.path().join("a.yml"),
        r#"
extends: b.yml
name: a
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("b.yml"),
        r#"
extends: a.yml
name: b
"#,
    )
    .unwrap();

    let err = load_spec(&dir.path().join("a.yml")).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("circular dependency"),
        "Expected circular error, got: {msg}"
    );
}

#[tokio::test]
async fn load_spec_extends_file_not_found() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(
        dir.path().join("child.yml"),
        r#"
extends: nonexistent.yml
name: child
"#,
    )
    .unwrap();

    let err = load_spec(&dir.path().join("child.yml")).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("base file not found"),
        "Expected not found error, got: {msg}"
    );
}

#[tokio::test]
async fn load_spec_extends_missing_required_fields() {
    let dir = tempfile::tempdir().unwrap();

    // Base provides target but no source
    std::fs::write(
        dir.path().join("base.yml"),
        r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#,
    )
    .unwrap();

    // Child adds name but no source — merged result is missing required `source`
    std::fs::write(
        dir.path().join("child.yml"),
        r#"
extends: base.yml
name: incomplete
"#,
    )
    .unwrap();

    let err = load_spec(&dir.path().join("child.yml")).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("source") || msg.contains("missing"),
        "Expected missing field error, got: {msg}"
    );
}

#[tokio::test]
async fn load_spec_extends_chain() {
    let dir = tempfile::tempdir().unwrap();

    // grandparent: provides target and assets
    std::fs::write(
        dir.path().join("grandparent.yml"),
        r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
cascade: false
build_timestamp: date
"#,
    )
    .unwrap();

    // parent: extends grandparent, overrides cascade
    std::fs::write(
        dir.path().join("parent.yml"),
        r#"
extends: grandparent.yml
cascade: true
skip_prereleases: true
"#,
    )
    .unwrap();

    // child: extends parent, adds name and source
    std::fs::write(
        dir.path().join("child.yml"),
        r#"
extends: parent.yml
name: chain-test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#,
    )
    .unwrap();

    let spec = load_spec(&dir.path().join("child.yml")).await.unwrap();
    assert_eq!(spec.name, "chain-test");
    assert_eq!(spec.target.registry, "ocx.sh");
    // cascade: grandparent=false, parent=true → true
    assert!(spec.cascade.enabled);
    // build_timestamp: grandparent=date, not overridden → date
    assert_eq!(spec.build_timestamp, BuildTimestampFormat::Date);
    // skip_prereleases: parent=true → true
    assert!(spec.skip_prereleases);
}

#[tokio::test]
async fn load_spec_extends_replaces_cascade_wholesale() {
    // `cascade` is one key, whichever shape it takes: a child spelling the
    // bool must not inherit the base's schedule, or opting a mirror out of
    // repair would leave it on a timer.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("base.yml"),
        r#"
target:
  registry: ocx.sh
  repository: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
cascade:
  schedule: "17 4 * * 1"
"#,
    )
    .unwrap();

    let child_body = r#"
extends: base.yml
name: chain-test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#;
    let child = dir.path().join("child.yml");

    std::fs::write(&child, child_body).unwrap();
    let inherited = load_spec(&child).await.unwrap();
    assert_eq!(inherited.cascade.schedule.as_deref(), Some("17 4 * * 1"));

    std::fs::write(&child, format!("{child_body}cascade: false\n")).unwrap();
    let overridden = load_spec(&child).await.unwrap();
    assert_eq!(
        overridden.cascade,
        CascadeConfig {
            enabled: false,
            schedule: None
        },
    );
}

#[tokio::test]
async fn load_spec_rejects_a_cron_that_could_add_its_own_triggers() {
    // Both cron fields are spliced into a generated workflow's `on:` block
    // inside a single-quoted scalar. A value that closes that scalar adds a
    // trigger of the spec's choosing — and any non-schedule trigger makes
    // the cascade repair run for real, unattended. Reject before render.
    let dir = tempfile::tempdir().unwrap();
    let body = r#"
name: cron-guard
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;
    let spec_path = dir.path().join("mirror.yml");

    for field in ["cascade:\n  schedule", "versions:\n  poll_interval"] {
        std::fs::write(
            &spec_path,
            format!("{body}{field}: \"0 4 * * 1'\\n  push:\\n    branches: [main]\\n#\"\n"),
        )
        .unwrap();
        let err = load_spec(&spec_path).await.expect_err("injected cron must be rejected");
        assert!(matches!(err, MirrorError::SpecInvalid(_)), "{field}: {err}");
    }

    std::fs::write(&spec_path, format!("{body}cascade:\n  schedule: \"17 4 * * 1\"\n")).unwrap();
    load_spec(&spec_path).await.expect("a plain cron must still load");
}
