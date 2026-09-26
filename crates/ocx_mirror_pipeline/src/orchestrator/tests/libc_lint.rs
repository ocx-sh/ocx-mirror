// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── libc_lint and dependency pins: create's checks, reached through prepare ──
//
// The libc check and the dependency pinning both run inside `ocx package
// create` now; what the mirror owns is handing create the right switch and the
// right input, and publishing what it compiled. The checks themselves are
// covered where they live, in ocx, and end to end by the acceptance suite.

/// `libc_lint: false` is create's `--no-libc-lint`, and the default passes
/// nothing — the check runs. A partial bypass on the mirror's side would be a
/// second opinion on a check the mirror no longer makes.
#[cfg(unix)]
#[tokio::test]
async fn the_libc_opt_out_reaches_create_as_its_own_flag() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");

    let checked = work.path().join("checked");
    prepare_offline(spec.path(), &checked, BinScanMode::Off, true)
        .await
        .expect("prepare succeeds");
    assert!(
        !create_invocations(&checked)[0].contains("--no-libc-lint"),
        "the default must leave the check on: {:?}",
        create_invocations(&checked),
    );

    let bypassed = work.path().join("bypassed");
    prepare_offline(spec.path(), &bypassed, BinScanMode::Off, false)
        .await
        .expect("prepare succeeds");
    assert!(
        create_invocations(&bypassed)[0].ends_with("--no-libc-lint"),
        "libc_lint: false must reach create: {:?}",
        create_invocations(&bypassed),
    );
}

/// Issue #90: a spec dependency named by tag alone is create's to pin, per
/// platform. The mirror used to project the authoring metadata itself and
/// refused the tag before create ever ran; now the tag-only file reaches
/// create as its `--metadata` input, and the pin create compiled is what both
/// publish paths carry.
#[cfg(unix)]
#[tokio::test]
async fn a_tag_only_dependency_reaches_create_and_its_pin_is_published() {
    const PINNED: &str =
        "ocx.sh/adoptium/temurin:jre-25@sha256:1111111111111111111111111111111111111111111111111111111111111111";

    let spec = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        spec.path().join("metadata.json"),
        r#"{"type":"bundle","version":1,
            "dependencies":[{"identifier":"ocx.sh/adoptium/temurin:jre-25","name":"temurin","visibility":"private"}]}"#,
    )
    .expect("write metadata fixture");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("task");
    std::fs::create_dir_all(&task_dir).expect("create task dir");
    std::fs::write(
        task_dir.join("fake-compiled.json"),
        format!(
            r#"{{"type":"bundle","version":1,
                "dependencies":[{{"identifier":"{PINNED}","name":"temurin","visibility":"private"}}]}}"#
        ),
    )
    .expect("stage the compiled sidecar");

    let metadata = prepare_offline(spec.path(), &task_dir, BinScanMode::Off, true)
        .await
        .expect("a tag-only dependency must not stop the prepare");

    let authoring = std::fs::read_to_string(task_dir.join("authoring-metadata.json")).expect("create's input");
    assert!(
        authoring.contains(r#""ocx.sh/adoptium/temurin:jre-25""#),
        "create must be handed the tag, unpinned: {authoring}",
    );
    let pinned: Vec<String> = metadata
        .dependencies()
        .iter()
        .map(|dependency| dependency.identifier.to_string())
        .collect();
    assert_eq!(pinned, [PINNED], "the in-process push carries create's pin");
    let sidecar = std::fs::read_to_string(task_dir.join("metadata.json")).expect("sidecar");
    assert!(sidecar.contains(PINNED), "and so does the sidecar CI pushes: {sidecar}");
}
