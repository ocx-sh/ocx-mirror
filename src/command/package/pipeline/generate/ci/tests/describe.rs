// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use std::path::Path;
use tempfile::tempdir;

// ── describe.yml renderer ──────────────────────────────────────────────

#[test]
fn render_minimal_spec_writes_describe_workflow() {
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-minimal.yml", dir.path());
    if let Ok(()) = result {
        let describe = dir.path().join(".github/workflows/describe.yml");
        assert!(describe.exists(), "describe.yml must be emitted alongside mirror.yml");
        let content = std::fs::read_to_string(&describe).unwrap();
        assert!(
            content.contains("name: describe"),
            "describe.yml must declare workflow name"
        );
        assert!(
            content.contains("ocx-mirror package pipeline describe"),
            "describe.yml must invoke `ocx-mirror package pipeline describe`"
        );
        assert!(content.contains("CATALOG.md"), "path filter must include CATALOG.md");
        assert!(
            content.contains("logo.*"),
            "path filter must include logo.* (svg/png probe target)"
        );
    }
}

#[test]
fn render_describe_uses_setup_ocx_action() {
    // After the setup-ocx migration the describe workflow no longer
    // installs ocx via `cargo install` from the submodule. It must use
    // the setup-ocx action and invoke `pipeline describe` directly
    // (setup-ocx activates the project toolchain onto PATH).
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-minimal.yml", dir.path());
    if let Ok(()) = result {
        let describe_path = dir.path().join(".github/workflows/describe.yml");
        let content = std::fs::read_to_string(&describe_path).unwrap();
        assert!(
            content.contains("uses: ocx-sh/setup-ocx@"),
            "describe workflow must install ocx via the setup-ocx action"
        );
        assert!(
            content.contains("ocx-mirror package pipeline describe"),
            "describe workflow must invoke pipeline describe directly (no `ocx exec --`)"
        );
        assert!(
            !content.contains("cargo install --path ocx/crates/ocx_mirror"),
            "describe workflow must not retain the legacy submodule install step"
        );
    }
}

#[test]
fn check_mode_detects_describe_yml_drift() {
    let dir = tempdir().unwrap();
    let spec = install_spec("mirror-minimal.yml", dir.path());

    let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

    if write_result.is_ok() {
        let describe_path = dir.path().join(".github/workflows/describe.yml");
        assert!(describe_path.exists(), "describe.yml must have been written");
        let mut content = std::fs::read_to_string(&describe_path).unwrap();
        content.push_str("\n# drift injection\n");
        std::fs::write(&describe_path, content).unwrap();

        let check_result = generate(dir.path(), &[spec], true);

        match check_result {
            Err(MirrorError::RendererDrift(paths)) => {
                assert!(
                    paths.iter().any(|p| p.contains("describe.yml")),
                    "drift must call out describe.yml: {paths:?}"
                );
            }
            Ok(()) => panic!("expected drift detection for describe.yml mutation"),
            Err(e) => panic!("expected RendererDrift, got: {e}"),
        }
    }
}

// ── verify-generated.yml drift-guard renderer ───────────────────────────────

#[test]
fn render_emits_verify_generated_drift_guard() {
    // Default render writes the drift-guard workflow that runs `--check`.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-minimal.yml", dir.path());
    if let Ok(()) = result {
        let verify = dir.path().join(".github/workflows/verify-generated.yml");
        assert!(verify.exists(), "verify-generated.yml must be emitted by default");
        let content = std::fs::read_to_string(&verify).unwrap();
        assert!(content.contains("DO NOT EDIT"), "must carry the DO-NOT-EDIT header");
        assert!(
            content.contains("uses: ocx-sh/setup-ocx@"),
            "drift guard must install ocx via the setup-ocx action"
        );
        assert!(
            content.contains("ocx-mirror package pipeline generate ci --check"),
            "drift guard must run `generate ci --check` directly (no `ocx exec --`)"
        );
        assert!(
            content.contains("pull_request:"),
            "drift guard must trigger on pull_request"
        );
    }
}

#[test]
fn verify_generated_emitted_by_default_in_render_map() {
    // Field absent → default false → drift guard present in the render map.
    let spec = spec_from_yaml(SHFMT_SPEC);
    let files = render(&[(root_slot(), spec)]);
    assert!(
        files.contains_key(Path::new(".github/workflows/verify-generated.yml")),
        "verify-generated.yml must be in the render map by default"
    );
}

#[test]
fn allow_manual_edits_skips_verify_generated() {
    // Opt-out: `allow_manual_edits: true` drops the drift guard but keeps the
    // two primary generated workflows.
    let spec = spec_from_yaml(&format!("{SHFMT_SPEC}allow_manual_edits: true\n"));
    let files = render(&[(root_slot(), spec)]);
    assert!(
        files.contains_key(Path::new(".github/workflows/mirror.yml")),
        "mirror.yml must still be rendered when opting out"
    );
    assert!(
        files.contains_key(Path::new(".github/workflows/describe.yml")),
        "describe.yml must still be rendered when opting out"
    );
    assert!(
        !files.contains_key(Path::new(".github/workflows/verify-generated.yml")),
        "verify-generated.yml must be skipped when allow_manual_edits is true"
    );
}

#[test]
fn check_mode_detects_verify_generated_drift() {
    // A hand-edit to verify-generated.yml itself must be caught by `--check`.
    let dir = tempdir().unwrap();
    let spec = install_spec("mirror-minimal.yml", dir.path());

    let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

    if write_result.is_ok() {
        let verify_path = dir.path().join(".github/workflows/verify-generated.yml");
        assert!(verify_path.exists(), "verify-generated.yml must have been written");
        let mut content = std::fs::read_to_string(&verify_path).unwrap();
        content.push_str("\n# drift injection\n");
        std::fs::write(&verify_path, content).unwrap();

        let check_result = generate(dir.path(), &[spec], true);

        match check_result {
            Err(MirrorError::RendererDrift(paths)) => {
                assert!(
                    paths.iter().any(|p| p.contains("verify-generated.yml")),
                    "drift must call out verify-generated.yml: {paths:?}"
                );
            }
            Ok(()) => panic!("expected drift detection for verify-generated.yml mutation"),
            Err(e) => panic!("expected RendererDrift, got: {e}"),
        }
    }
}

#[test]
fn verify_generated_template_runs_check_command() {
    let template = VERIFY_GENERATED_TEMPLATE;
    assert!(
        template.contains("ocx-mirror package pipeline generate ci --check"),
        "drift-guard template must invoke `generate ci --check`"
    );
    assert!(
        template.contains("DO NOT EDIT"),
        "drift-guard template must carry the DO-NOT-EDIT header"
    );
}
