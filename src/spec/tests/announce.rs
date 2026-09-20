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
    assert_eq!(announce.fork.as_deref(), Some("ocx-contrib/index"));
    assert_eq!(announce.index_repo, DEFAULT_INDEX_REPO);
    assert!(
        spec.validate(Path::new("test.yml")).is_empty(),
        "valid announce block must not error"
    );
}

/// The GitLab job-token shape: no fork, a self-hosted nested-group index
/// coordinate, the forge and the git transport spelled out.
#[test]
fn announce_block_accepts_a_self_hosted_gitlab_coordinate_without_a_fork() {
    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  index_repo: gitlab.corp.example/tools/ocx/index
  forge: gitlab
  transport: git
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let announce = spec.announce.as_ref().expect("announce block parsed");
    assert_eq!(announce.fork, None);
    assert_eq!(announce.transport(), crate::spec::WriteTransport::Git);
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.is_empty(),
        "a nested GitLab group path is a valid coordinate: {errors:?}"
    );
}

#[test]
fn validate_rejects_an_unknown_forge_or_transport_and_git_on_github() {
    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  forge: gitea
  transport: ssh
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(errors.iter().any(|e| e.contains("announce.forge")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("announce.transport")), "{errors:?}");

    // Spelled out, and inferred: the default `ocx-sh/index` has no host, which
    // `ocx` reads as github.com.
    for forge_line in ["  forge: github\n", ""] {
        let yaml = format!(
            r#"{base}
announce:
  package: bazelbuild/bazelisk
{forge_line}  transport: git
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors.iter().any(|e| e.contains("GitLab-only")),
            "the git transport exists for GitLab job tokens; on GitHub ocx refuses it at exit 64: {errors:?}"
        );
    }
    // gitlab.com needs no `forge:` — the host says so.
    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  index_repo: gitlab.com/ocx-mirrors/index
  transport: git
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(errors.is_empty(), "{errors:?}");
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

// ── parity with `ocx package announce`'s own exit-64 refusals ────────

/// R-H1(a): a self-hosted index host says nothing about what runs there, so
/// `ocx` refuses it without `--forge` under every transport — the spec must
/// too, naming `announce.forge` as the remedy rather than the old
/// GitLab-only wording that only the git transport produced.
#[test]
fn validate_refuses_a_self_hosted_index_without_a_forge_under_any_transport() {
    for transport_line in ["", "  transport: api\n", "  transport: git\n"] {
        let yaml = format!(
            r#"{base}
announce:
  package: bazelbuild/bazelisk
  index_repo: git.corp.example/tools/index
{transport_line}"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot tell which forge 'git.corp.example' is")
                    && e.contains("set announce.forge to 'github' or 'gitlab'")),
            "transport line {transport_line:?}: ocx refuses a self-hosted host with no --forge: {errors:?}"
        );
    }
}

/// R-H1(b): the git transport pushes the claim branch to the index
/// repository itself, so a fork has nothing to do — `ocx` refuses the pair
/// naming both flags, and the spec names both keys.
#[test]
fn validate_refuses_the_git_transport_together_with_a_fork() {
    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  index_repo: gitlab.com/ocx-mirrors/index
  fork: gitlab.com/me/index
  transport: git
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("announce.transport: 'git'") && e.contains("drop announce.fork")),
        "git transport with a fork must be refused naming both keys: {errors:?}"
    );
}

/// R-H1(c): GitHub has no nested namespaces. The coordinate grammar accepts
/// `acme/platform/index` (it is a GitLab group path), so only the resolved
/// forge can refuse it — `ForgeKind::validate_coordinate`, for either key.
#[test]
fn validate_refuses_a_nested_namespace_on_github() {
    for (field, announce) in [
        ("index_repo", "  index_repo: github.com/acme/platform/index\n"),
        ("fork", "  fork: acme/platform/index\n"),
    ] {
        let yaml = format!(
            r#"{base}
announce:
  package: bazelbuild/bazelisk
{announce}"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with(&format!("announce.{field}: ")) && e.contains("nested")),
            "a nested namespace on GitHub must be refused on announce.{field}: {errors:?}"
        );
    }
}

/// R-H1(d): a fork lives on the same instance as its upstream. An omitted
/// host means the forge's canonical host, so `ocx-sh/index` with
/// `fork: github.com/me/index` names one instance twice and stays valid;
/// two different hosts are refused naming both.
#[test]
fn validate_refuses_a_fork_on_a_different_host_than_the_index() {
    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  index_repo: gitlab.com/ocx-mirrors/index
  fork: gitlab.corp.example/me/index
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("announce.fork") && e.contains("gitlab.corp.example") && e.contains("gitlab.com")),
        "a fork on another host must be refused naming both hosts: {errors:?}"
    );

    let yaml = format!(
        r#"{base}
announce:
  package: bazelbuild/bazelisk
  fork: github.com/me/index
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.is_empty(),
        "an omitted host is the canonical host — github.com spelled out is the same instance: {errors:?}"
    );
}

/// Patch-release guard: every shape that validated before the parity fix
/// still does — the default index with a fork, and a nested gitlab.com group
/// path with the forge inferred and the git transport.
#[test]
fn previously_valid_announce_shapes_still_validate() {
    for announce in [
        "  fork: ocx-contrib/index\n",
        "  index_repo: gitlab.com/group/sub/index\n  transport: git\n",
        "  index_repo: gitlab.com/group/sub/index\n  fork: gitlab.com/me/index\n",
        "  index_repo: git.corp.example/tools/index\n  forge: gitlab\n  transport: git\n",
        "  index_repo: ghe.corp.example/tools/index\n  forge: github\n  fork: ghe.corp.example/me/index\n",
    ] {
        let yaml = format!(
            r#"{base}
announce:
  package: bazelbuild/bazelisk
{announce}"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors = spec.validate(Path::new("test.yml"));
        assert!(errors.is_empty(), "{announce:?} must stay valid: {errors:?}");
    }
}
