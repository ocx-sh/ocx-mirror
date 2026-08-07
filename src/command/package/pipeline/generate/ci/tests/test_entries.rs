// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── §TestEntry union: CI render tests ──────────────────────────────────────

/// Build a `MirrorSpec` from inline YAML and call `build_matrix` on it.
fn build_matrix_from_yaml(yaml: &str) -> Vec<MatrixLeg> {
    let spec: crate::spec::MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    build_matrix(&spec)
}

#[test]
fn render_matrix_entries_emits_kind_command() {
    // A spec with `command:` must produce `kind: command` + `command: <value>` in matrix.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-minimal.yml", dir.path());
    if let Ok(()) = result {
        let workflow = dir.path().join(".github/workflows/mirror.yml");
        let content = std::fs::read_to_string(&workflow).unwrap();
        assert!(
            content.contains("kind: command"),
            "matrix entry for command test must contain 'kind: command'; content:\n{content}"
        );
        assert!(
            content.contains("command: shfmt --version"),
            "matrix entry must contain 'command: shfmt --version'; content:\n{content}"
        );
    }
}

#[test]
fn render_matrix_entries_emits_kind_script() {
    // A spec with `script:` must produce `kind: script` + `script: <path>` in matrix.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
    if let Ok(()) = result {
        let workflow = dir.path().join(".github/workflows/mirror.yml");
        let content = std::fs::read_to_string(&workflow).unwrap();
        assert!(
            content.contains("kind: script"),
            "matrix entry for script test must contain 'kind: script'; content:\n{content}"
        );
        assert!(
            content.contains("script: tests/smoke.star"),
            "matrix entry must contain 'script: tests/smoke.star'; content:\n{content}"
        );
    }
}

#[test]
fn render_matrix_entries_emits_kind_script_inline() {
    // A spec with `script_inline:` must produce `kind: script_inline` with YAML
    // block scalar (`script_inline: |`) in the matrix entry.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
    if let Ok(()) = result {
        let workflow = dir.path().join(".github/workflows/mirror.yml");
        let content = std::fs::read_to_string(&workflow).unwrap();
        assert!(
            content.contains("kind: script_inline"),
            "matrix entry for inline test must contain 'kind: script_inline'; content:\n{content}"
        );
        assert!(
            content.contains("script_inline: |"),
            "inline test payload must use YAML block scalar ('script_inline: |'); content:\n{content}"
        );
    }
}

#[test]
fn render_all_three_kinds_in_single_spec() {
    // All three kinds must co-exist in the same matrix.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
    if let Ok(()) = result {
        let workflow = dir.path().join(".github/workflows/mirror.yml");
        let content = std::fs::read_to_string(&workflow).unwrap();
        assert!(content.contains("kind: command"), "command kind missing");
        assert!(content.contains("kind: script"), "script kind missing");
        assert!(content.contains("kind: script_inline"), "script_inline kind missing");
    }
}

#[test]
fn shell_loop_branches_on_test_kind() {
    // The generated shell loop must extract TEST_KIND and branch on its
    // value (command / script / script_inline). Native-only after the
    // setup-ocx migration — container path is exercised via the upstream
    // rejection test (`render_rejects_container_legs_with_usage_error`).
    let legs = build_matrix_from_yaml(
        r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
tests:
  - name: version
    command: shfmt --version
  - name: smoke
    script: tests/smoke.star
  - name: inline
    script_inline: |
      ocx_assert(True)
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
    );
    let shell_block = render_test_run_steps(&legs, false);

    // Must extract TEST_KIND.
    assert!(
        shell_block.contains("TEST_KIND=$(echo \"${TESTS_JSON}\" | jq -r \".[$i].kind\" | tr -d '\\r')"),
        "shell loop must extract TEST_KIND; block:\n{shell_block}"
    );
    // Must branch on command.
    assert!(
        shell_block.contains("if [ \"${TEST_KIND}\" = \"command\" ]"),
        "shell loop must have command branch; block:\n{shell_block}"
    );
    // Must branch on script.
    assert!(
        shell_block.contains("elif [ \"${TEST_KIND}\" = \"script\" ]"),
        "shell loop must have script branch; block:\n{shell_block}"
    );
    // Must handle script_inline via else branch (includes printf piped to --script -).
    assert!(
        shell_block.contains("--script -"),
        "shell loop must pipe script_inline to --script -; block:\n{shell_block}"
    );
    // Native script: uses --script $TEST_SCRIPT (not -c).
    assert!(
        shell_block.contains("--script \"${TEST_SCRIPT}\""),
        "native script branch must pass --script; block:\n{shell_block}"
    );
    // Every `ocx package test` invocation in the loop is called directly —
    // setup-ocx activates the project toolchain onto PATH for the step.
    assert!(
        shell_block.contains("ocx package test"),
        "every ocx package test invocation must be called directly (no `ocx run --`); block:\n{shell_block}"
    );
    assert!(
        !shell_block.contains("ocx run"),
        "test loop must not wrap `ocx package test` in `ocx run`; block:\n{shell_block}"
    );
    // No leftover docker injection from the previous container shape.
    assert!(
        !shell_block.contains("docker run"),
        "native-only renderer must not emit any `docker run` lines; block:\n{shell_block}"
    );
}

// Regression: native jq.exe on Windows runners emits CRLF, so without
// `tr -d '\r'` after each jq pipeline in the test job the captured
// `${VERSION}` carried a trailing CR and corrupted bundle paths
// (e.g. `bundles/bundle-3.10.0\r-windows_amd64.tar.xz`).
#[test]
fn workflow_template_strips_cr_after_jq_for_windows_runners() {
    let template = WORKFLOW_TEMPLATE;
    assert!(
        template.contains("jq -r '.[].version' | tr -d '\\r'"),
        "test job must strip CR from jq output to survive Git Bash + native jq.exe on Windows"
    );
    assert!(
        template.contains("head -n1 | tr -d '\\r' || true"),
        "CI_JOB_URL capture must strip CR before exporting the URL"
    );
}

// Regression (live W4 pypi pilot, mirror-pypi run 30874908824 job
// 91884431517): the env leg's `test_target_resolve_script` captured layer
// paths straight out of `jq -r`, and on windows-latest each captured value
// kept a trailing CR — Git Bash word-splits `$()` on LF only. `ocx package
// test` then received `…/<digest>.tar.zst\r` and died with os error 123
// ("The filename, directory name, or volume label syntax is incorrect"),
// reddening the leg's JUnit and withholding the windows index entry. The
// raw job log shows the CR verbatim, and the uploaded env-manifest.json is
// CR-free, so the CR is injected on the runner, not carried in the data.
//
// Asserted structurally rather than as a golden byte-diff: the invariant is
// "no jq capture in a Windows-reachable script escapes without `tr -d '\r'`",
// which must hold for jq pipelines added later too.
#[test]
fn every_jq_capture_in_the_test_job_scripts_strips_cr() {
    let legs = build_matrix_from_yaml(
        r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
tests:
  - name: version
    command: shfmt --version
  - name: smoke
    script: tests/smoke.star
  - name: inline
    script_inline: |
      ocx_assert(True)
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
    );

    // The version-matrix loop lives in the template itself; slice out its
    // step so the discover/summarize jobs (ubuntu-only, never Windows) do
    // not count.
    let version_matrix_loop = WORKFLOW_TEMPLATE
        .split_once("- name: Run tests for all versions")
        .map(|(_, rest)| rest.split("\n      - ").next().unwrap_or(rest))
        .expect("template carries the test job's version-matrix loop");

    // Every script the test job runs on a windows-latest leg: the
    // version-matrix loop, the shared per-test loop, and both
    // target-resolve variants (env and archive).
    let scripts = [
        ("version matrix loop", version_matrix_loop.to_string()),
        ("test run steps", render_test_run_steps(&legs, false)),
        ("env target resolve", test_target_resolve_script(true).to_string()),
        ("archive target resolve", test_target_resolve_script(false).to_string()),
    ];

    let mut scanned = 0;
    for (label, script) in &scripts {
        // Fold shell continuations so a pipeline wrapped across lines is
        // scanned as the single logical line it runs as.
        let folded = script.replace("\\\n", " ");
        for line in folded.lines() {
            if !line.contains("jq ") {
                continue;
            }
            scanned += 1;
            assert!(
                line.contains("tr -d '\\r'"),
                "{label}: jq output is captured by the shell without stripping CR, \
                 which corrupts paths on windows-latest; line:\n{line}"
            );
        }
    }
    // Non-vacuity: 3 in the version-matrix loop, 6 in the per-test loop,
    // 3 in the env resolve block. A restructure that empties a slice must
    // fail here rather than pass by scanning nothing.
    assert_eq!(scanned, 12, "expected every known jq capture to be scanned");
}

// ── Per-version platform-set filter in the test loop ──────────────────────

#[test]
fn workflow_test_loop_skips_versions_outside_platform_set() {
    // The test loop must skip versions whose declared platform set excludes
    // this matrix leg's platform — fixes the backfill-partial false-red and
    // never re-tests out-of-window / excluded `(V, P)` pairs.
    let template = WORKFLOW_TEMPLATE;
    assert!(
        template.contains("select(.version == $v) | .platforms | index($p)"),
        "test loop must membership-check matrix.platform against the version's platform set"
    );
    assert!(
        template.contains("if [ -z \"${IN_SET}\" ]; then"),
        "test loop must `continue` when the platform is not in the version's set"
    );
}
