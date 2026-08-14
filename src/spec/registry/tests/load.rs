// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `load_registry_spec` — C-007.
//!
//! The contract is an **order**, and every test here pins one step of it:
//! absent → 79, merge, pre-scan → 64, deserialize → 65, validate → 65. Two of
//! them are the reason the order is written down at all — a document rejected
//! by the pre-scan must never reach serde, and `kind:` must never reach it
//! either.
//!
//! The `load_spec` regression test at the bottom belongs here rather than
//! beside `MirrorSpec`: it exists to prove *this* work left the package-mirror
//! loader alone.

use ocx_lib::cli::ExitCode;

use super::support::*;
use crate::error::MirrorError;
use crate::spec::{load_registry_spec, load_spec};

/// A `mirror.yml` with no `kind:` — the shape every existing spec in this
/// repository has, kept minimal so it exercises the loader and not the
/// package-mirror schema.
const MIRROR_YAML: &str = r#"
name: cmake
target:
  registry: ocx.sh
  repository: kitware/cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*-linux-x86_64\\.tar\\.gz"
"#;

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

// ── The happy path, and the `kind` strip it depends on ──────────────────────

/// The document an operator actually writes — `kind: registry` and all — must
/// load.
///
/// This is the guard on C-007's `kind` strip. `RegistrySpec` carries
/// `deny_unknown_fields` and has no `kind` field, so without the strip this
/// test fails on **every** valid `registry.yml`, and so does every acceptance
/// scenario downstream of it.
#[tokio::test]
async fn an_operator_written_registry_yml_loads_with_kind_read_then_stripped() {
    let dir = temp_dir();
    let path = write_spec(dir.path(), "registry.yml", &valid_registry_yaml());

    let spec = load_registry_spec(&path)
        .await
        .expect("a valid registry.yml must load, `kind:` included");

    assert_eq!(spec.target.repository, "mirror");
    assert_eq!(spec.sources[0].as_name(), "upstream");
}

// ── Step 1 — the file has to be there ───────────────────────────────────────

#[tokio::test]
async fn an_absent_spec_is_not_found() {
    let dir = temp_dir();

    let error = load_registry_spec(&dir.path().join("registry.yml"))
        .await
        .expect_err("an absent spec cannot load");

    assert!(matches!(error, MirrorError::SpecNotFound(_)), "{error}");
    assert_eq!(error.kind_exit_code(), ExitCode::NotFound);
}

// ── Step 2 — the pre-scan, before serde ─────────────────────────────────────

/// The ordering, stated as the one case that can tell the two apart: a
/// document that is **both** credential-bearing and structurally invalid must
/// report the credential, at 64.
///
/// Run the other way round, serde would reject it for its missing `sources:`
/// and the operator would fix that, re-run, and only then learn the real
/// problem — with the secret sitting in the file the whole time.
#[tokio::test]
async fn a_credential_is_reported_before_the_document_is_deserialized() {
    let dir = temp_dir();
    let path = write_spec(
        dir.path(),
        "registry.yml",
        "kind: registry\ntarget:\n  registry: localhost:5002\n  password: hunter2\n",
    );

    let error = load_registry_spec(&path).await.expect_err("a credential is refused");

    assert!(matches!(error, MirrorError::SpecUsageError(_)), "{error}");
    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    let rendered = error.to_string();
    assert!(rendered.contains("target.password"), "{rendered}");
    assert!(
        !rendered.contains("hunter2"),
        "the value must never reach a message: {rendered}"
    );
}

/// **The pre-scan's userinfo job, asserted through the loader.**
///
/// `catalog.rs` argues from this: it never echoes an index URL's authority
/// because a credential-bearing one cannot reach a fetch. That argument is
/// worth nothing if the pre-scan is merely *callable* — it has to be on the
/// path a run actually takes, which is what this test and its credential
/// sibling above pin. Unit tests over `pre_scan` in isolation cannot.
#[tokio::test]
async fn a_userinfo_bearing_index_url_is_refused_at_load() {
    let dir = temp_dir();
    let path = write_spec(
        dir.path(),
        "registry.yml",
        &valid_registry_yaml().replace(
            "index: https://index.example/",
            "index: https://operator:hunter2@index.example/",
        ),
    );

    let error = load_registry_spec(&path)
        .await
        .expect_err("an index URL carrying credentials is refused");

    assert!(matches!(error, MirrorError::SpecUsageError(_)), "{error}");
    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    let rendered = error.to_string();
    assert!(rendered.contains("sources[0].index"), "{rendered}");
    assert!(
        !rendered.contains("hunter2"),
        "the embedded password must never reach a message: {rendered}"
    );
}

/// A `mirror.yml` handed to `registry sync` is diagnosed by its missing
/// `kind:`, not by whichever schema difference serde happens to hit first.
#[tokio::test]
async fn a_package_mirror_spec_handed_to_the_registry_loader_is_diagnosed_by_kind() {
    let dir = temp_dir();
    let path = write_spec(dir.path(), "mirror.yml", MIRROR_YAML);

    let error = load_registry_spec(&path)
        .await
        .expect_err("a mirror.yml is not a registry spec");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    let rendered = error.to_string();
    assert!(rendered.contains("kind"), "{rendered}");
    assert!(
        !rendered.contains("sources"),
        "the missing `sources:` is a consequence, not the diagnosis: {rendered}"
    );
}

// ── Step 2 runs on the MERGED document ──────────────────────────────────────

/// The pre-scan is post-merge, so a `kind:` an `extends:` base supplies is
/// read exactly as one written in the child.
#[tokio::test]
async fn kind_supplied_by_an_extends_base_satisfies_the_discriminator() {
    let dir = temp_dir();
    write_spec(dir.path(), "base.yml", &format!("{KIND_LINE}output: public\n"));
    let path = write_spec(
        dir.path(),
        "registry.yml",
        &format!("extends: base.yml\n{}", VALID_BODY.replace("output: public\n", "")),
    );

    let spec = load_registry_spec(&path).await.expect("the merged document is valid");

    assert_eq!(spec.output, std::path::PathBuf::from("public"));
}

/// …and a credential hidden in a base is caught with no chain-walking of its
/// own, because the scan runs after the merge rather than per file.
#[tokio::test]
async fn a_credential_hidden_in_an_extends_base_is_still_caught() {
    let dir = temp_dir();
    write_spec(dir.path(), "base.yml", "token: ghp_example\n");
    let path = write_spec(
        dir.path(),
        "registry.yml",
        &format!("extends: base.yml\n{}", valid_registry_yaml()),
    );

    let error = load_registry_spec(&path)
        .await
        .expect_err("a credential in the base is still a credential");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(error.to_string().contains("token"), "{error}");
}

// ── Steps 3 and 4 — deserialize, then validate ──────────────────────────────

#[tokio::test]
async fn a_yaml_parse_error_is_a_data_error() {
    let dir = temp_dir();
    let path = write_spec(dir.path(), "registry.yml", "kind: registry\n  output: [unterminated\n");

    let error = load_registry_spec(&path).await.expect_err("malformed YAML cannot load");

    assert!(matches!(error, MirrorError::SpecInvalid(_)), "{error}");
    assert_eq!(error.kind_exit_code(), ExitCode::DataError);
}

#[tokio::test]
async fn an_unknown_top_level_key_is_a_data_error() {
    let dir = temp_dir();
    let path = write_spec(
        dir.path(),
        "registry.yml",
        &format!("{}\nversions:\n  min: 1.0.0\n", valid_registry_yaml()),
    );

    let error = load_registry_spec(&path).await.expect_err("an unknown key is refused");

    assert_eq!(error.kind_exit_code(), ExitCode::DataError);
}

#[tokio::test]
async fn a_validation_failure_carries_every_message_at_the_data_error_code() {
    let dir = temp_dir();
    let path = write_spec(
        dir.path(),
        "registry.yml",
        &format!(
            "{KIND_LINE}{}",
            VALID_BODY.replace("sources:", "concurrency:\n  max_blobs: 0\nsources:")
        ),
    );

    let error = load_registry_spec(&path)
        .await
        .expect_err("a spec failing validation cannot load");

    let MirrorError::SpecInvalid(messages) = &error else {
        panic!("a validation failure is SpecInvalid: {error}");
    };
    assert!(
        messages.iter().any(|message| message.contains("max_blobs")),
        "{messages:?}"
    );
    assert_eq!(error.kind_exit_code(), ExitCode::DataError);
}

// ── The regression this whole work package must not cause ───────────────────

/// **`load_spec` is not modified and the pre-scan is not wired into it.**
///
/// C-005's `kind` job makes an absent discriminator a hard exit 64. Wired into
/// the shared loader instead of into `load_registry_spec` alone, every
/// `mirror.yml` in existence — none of which carries `kind:` — would stop
/// loading. The acceptance corpus would go red too, but it would not say why.
#[tokio::test]
async fn a_kind_less_mirror_yml_still_loads_through_load_spec() {
    let dir = temp_dir();
    let path = write_spec(dir.path(), "mirror.yml", MIRROR_YAML);

    let spec = load_spec(&path)
        .await
        .expect("a `mirror.yml` carries no `kind:` and must keep loading without one");

    assert_eq!(spec.name, "cmake");
    assert_eq!(spec.target.repository, "kitware/cmake");
}

/// The same guarantee one level down: `load_spec` still resolves an `extends:`
/// chain, which is the half of it the loader split actually moved.
#[tokio::test]
async fn load_spec_still_resolves_an_extends_chain() {
    let dir = temp_dir();
    write_spec(dir.path(), "base.yml", MIRROR_YAML);
    let path = write_spec(dir.path(), "mirror.yml", "extends: base.yml\nname: cmake-nightly\n");

    let spec = load_spec(&path)
        .await
        .expect("the merged document is a valid mirror spec");

    assert_eq!(spec.name, "cmake-nightly", "the child's key wins");
    assert_eq!(spec.target.repository, "kitware/cmake", "the base's keys survive");
}
