// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── Env-package sources (pylock / pypi) ───────────────────────────────────

#[test]
fn an_env_spec_gathers_the_version_subtree_and_tests_the_composed_layers() {
    // A `source.type: pylock` spec publishes an env package, so two joints
    // of the workflow change shape: prepare copies the whole per-version
    // subtree into `bundles/{V}/` (there is no per-platform bundle.tar.xz to
    // flatten), and the test job resolves that version's env-manifest.json
    // into a `-m <metadata> <layers…>` invocation.
    let content = workflow_for("mirror-pylock.yml");

    assert!(
        content.contains(r#"cp -R ".ocx-mirror/${V_SLUG}" "bundles/${V}""#),
        "prepare must copy the version env subtree into bundles/:\n{content}"
    );
    assert!(
        !content.contains("bundle.tar.xz"),
        "an env workflow must carry no archive bundle flattening:\n{content}"
    );
    assert!(
        content.contains("env-manifest.json"),
        "the test job must read the env manifest:\n{content}"
    );
    assert!(
        content.contains(r#"-m "${METADATA}" ${LAYERS}"#),
        "`ocx package test` must receive the composed metadata + ordered layers:\n{content}"
    );
    // A genuine miss — a version whose prepare leg failed and uploaded no
    // manifest — must red that one version through the `|| RC=$?` capture,
    // not trip `set -e` and abort every remaining version with no JUnit.
    assert!(
        content.contains(r#""${VERSION_DIR}/env-manifest.json" 2>/dev/null | tr -d '\r' || true"#),
        "manifest resolution must tolerate a missing manifest:\n{content}"
    );
    // `set -u` is in force: an env leg sets neither variable, so naming
    // either one would abort the step before the first test runs.
    assert!(
        !content.contains(r#""${BUNDLE}""#) && !content.contains("METADATA_SIBLING"),
        "an env workflow must not reference the archive BUNDLE variables:\n{content}"
    );
    // A container leg declares its own libc as an os_feature, so the env's
    // private interpreter resolves against a per-libc index entry.
    assert!(
        content.contains(r#"--platform "${TEST_PLATFORM}""#)
            && content.contains(r#"musl) TEST_PLATFORM="${TEST_PLATFORM}+libc.musl" ;;"#),
        "env test invocations must declare the leg's libc:\n{content}"
    );
    // A committed lock needs no in-pipeline derivation, so the plan
    // artifact stays exactly what every other source uploads.
    assert!(
        content.contains("          path: plan.json\n") && !content.contains("derived-locks"),
        "a pylock spec must keep the single-path plan artifact:\n{content}"
    );
}

#[test]
fn an_archive_spec_still_flattens_bundles_and_tests_one_file() {
    // The other half of the env split: an archive/binary spec must render
    // the bundle flatten and the single-bundle test target it always has.
    let content = workflow_for("mirror-minimal.yml");

    assert!(
        content.contains(r#"cp "${platform_dir}bundle.tar.xz""#),
        "archive prepare must still flatten bundle.tar.xz:\n{content}"
    );
    assert!(
        content.contains(r#"BUNDLE="bundles/bundle-${VERSION}-${{ matrix.platform_slug }}.tar.xz""#),
        "the archive test job must still resolve the single bundle path:\n{content}"
    );
    assert!(
        content.contains(r#"METADATA_SIBLING="${BUNDLE%.tar.xz}-metadata.json""#),
        "the archive test job must still name the metadata sibling:\n{content}"
    );
    assert!(
        !content.contains("env-manifest.json"),
        "an archive workflow must carry no env-manifest logic:\n{content}"
    );
    assert!(
        content.contains("          path: plan.json\n") && !content.contains("derived-locks"),
        "an archive spec must keep the single-path plan artifact:\n{content}"
    );
}

#[test]
fn a_pypi_spec_ships_its_derived_locks_to_prepare_and_to_the_audit_trail() {
    // `pypi` derives a PEP 751 lock per version during the plan phase, so
    // `locks/` must travel to prepare inside the plan artifact, and a
    // second long-retention copy outlives that artifact's single day.
    let content = workflow_for("mirror-pypi.yml");

    assert!(
        content
            .contains("          name: plan\n          path: |\n            plan.json\n            locks/\n          retention-days: 1\n"),
        "the plan artifact must carry both plan.json and locks/:\n{content}"
    );
    assert!(
        content.contains(
            "          name: derived-locks\n          path: locks/\n          retention-days: 90\n          if-no-files-found: ignore\n"
        ),
        "a pypi workflow must carry the 90-day derived-locks artifact:\n{content}"
    );
    assert!(
        content.contains("env-manifest.json"),
        "pypi is an env source too, so the test job reads the env manifest:\n{content}"
    );
}

#[test]
fn the_derived_locks_upload_tracks_the_templates_action_pin() {
    // The audit upload is rendered from Rust, so its `uses:` line is read
    // back out of the template: a literal here would sit outside the
    // Renovate customManager, which only scans `templates/*.yml`.
    let template_pin = WORKFLOW_TEMPLATE
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("- uses: actions/upload-artifact@"))
        .expect("the workflow template pins actions/upload-artifact");
    assert!(
        derived_locks_artifact(true).contains(template_pin),
        "the derived-locks upload must reuse the template's pinned action, got:\n{}",
        derived_locks_artifact(true)
    );
}
