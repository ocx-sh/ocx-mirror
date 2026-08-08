// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::error::MirrorError;

// ── announce ──────────────────────────────────────────────────────────

#[test]
fn announce_block_round_trips_and_defaults_the_index_repo() {
    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  fork: ocx-contrib/index
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let announce = spec.announce.as_ref().expect("announce block parsed");
    assert_eq!(announce.package, "bazelbuild/bazelisk");
    assert_eq!(announce.fork, "ocx-contrib/index");
    assert_eq!(announce.index_repo, DEFAULT_INDEX_REPO);
    assert!(
        spec.validate(Path::new("test.yml")).is_empty(),
        "valid announce block must not error"
    );
}

#[test]
fn spec_without_announce_block_announces_nothing() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(MINIMAL_BASE_YAML).unwrap();
    assert!(spec.announce.is_none(), "announce is opt-in");
}

#[test]
fn validate_rejects_malformed_announce_package_with_a_named_error() {
    // A bare package name is the likely mistake — the index needs the
    // `<namespace>/<package>` pair, and the message has to say which
    // field is wrong rather than surface a serde shape mismatch.
    let yaml = format!(
        r#"{base}
announce:
  package: bazelisk
  fork: ocx-contrib/index
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("announce.package") && e.contains("<namespace>/<package>")),
        "malformed package must produce a named field error: {errors:?}"
    );
}

#[test]
fn validate_rejects_malformed_announce_fork_and_index_repo() {
    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  fork: https://github.com/ocx-contrib/index
  index_repo: index
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("announce.fork")),
        "URL paste into fork must error: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("announce.index_repo")),
        "bare repo name must error: {errors:?}"
    );
}

#[tokio::test]
async fn load_spec_rejects_an_announce_cron_that_could_add_its_own_triggers() {
    // `announce.schedule` is spliced into the generated workflow's `on:`
    // block inside a single-quoted scalar, exactly as the other two cron
    // fields are. A value that closes that scalar adds a trigger of the
    // spec's choosing — and a scheduled announce opens index pull requests
    // for real. Reject before render, naming the field to go fix.
    let dir = tempfile::tempdir().unwrap();
    let body = r#"
name: announce-cron-guard
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
announce:
  package: test/test
  fork: ocx-contrib/index
"#;
    let spec_path = dir.path().join("mirror.yml");

    std::fs::write(
        &spec_path,
        format!("{body}  schedule: \"0 4 * * 1'\\n  push:\\n    branches: [main]\\n#\"\n"),
    )
    .unwrap();
    match load_spec(&spec_path).await.expect_err("injected cron must be rejected") {
        MirrorError::SpecInvalid(errors) => assert!(
            errors.iter().any(|e| e.contains("announce.schedule")),
            "the error must name the field: {errors:?}"
        ),
        other => panic!("expected SpecInvalid, got: {other}"),
    }

    std::fs::write(&spec_path, format!("{body}  schedule: \"23 5 * * 2\"\n")).unwrap();
    let spec = load_spec(&spec_path).await.expect("a plain cron must still load");
    assert_eq!(
        spec.announce.expect("announce block parsed").schedule.as_deref(),
        Some("23 5 * * 2")
    );
}
