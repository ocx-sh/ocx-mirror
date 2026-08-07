// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use std::path::Path;
use tempfile::tempdir;

// ── `script:` paths ───────────────────────────────────────────────────────

/// A spec whose one test is a Starlark `script:`, declared top-level.
fn script_spec(script: &str) -> String {
    format!(
        "{SHFMT_SPEC}platforms:\n  linux/amd64:\n    runner: ubuntu-latest\ntests:\n  - name: smoke\n    script: {script}\n"
    )
}

/// The same, declared as a per-platform override — a second list of tests
/// that nothing else validates.
fn platform_script_spec(script: &str) -> String {
    format!(
        "{SHFMT_SPEC}platforms:\n  linux/amd64:\n    runner: ubuntu-latest\n    tests:\n      - name: smoke\n        script: {script}\n"
    )
}

/// Render a one-spec repository at `buildifier/mirror.yml`, optionally
/// creating a script file at `create` first.
fn render_with_script(spec_yaml: &str, create: Option<&str>) -> Result<(), MirrorError> {
    let dir = tempdir().unwrap();
    if let Some(at) = create {
        write_file(dir.path(), at, "ocx_assert(True)\n");
    }
    let spec = write_file(dir.path(), "buildifier/mirror.yml", spec_yaml);
    generate(dir.path(), &[spec], false)
}

/// The one message of a rejected render, or a panic naming what came back.
fn only_spec_error(result: Result<(), MirrorError>) -> String {
    match result {
        Err(MirrorError::SpecInvalid(errors)) => {
            assert_eq!(errors.len(), 1, "one missing script, one message: {errors:?}");
            errors.into_iter().next().expect("just asserted one")
        }
        other => panic!("a missing test script must be a spec error, got: {other:?}"),
    }
}

#[test]
fn a_test_script_that_does_not_exist_is_a_spec_error() {
    // Rendering a workflow that names a script nobody wrote is a green here
    // and a red test leg in someone else's CI run, after a publish attempt.
    render_with_script(
        &script_spec("buildifier/tests/smoke.star"),
        Some("buildifier/tests/smoke.star"),
    )
    .expect("a spec whose script exists must render");

    let missing = only_spec_error(render_with_script(&script_spec("buildifier/tests/smoke.star"), None));
    assert!(
        missing.contains("entry 'smoke' script not found: buildifier/tests/smoke.star")
            && missing.contains("resolves from the repository root as "),
        "the message must name the path and what it resolved against, got: {missing}"
    );
    assert!(
        !missing.contains("write "),
        "nothing exists anywhere, so there is no better path to suggest, got: {missing}"
    );

    // The near miss: `tests/smoke.star` inside `buildifier/mirror.yml` reads
    // as spec-relative and means repo-root-relative. Saying only "not found"
    // would leave the author staring at a file that is right there.
    let near_miss = only_spec_error(render_with_script(
        &script_spec("tests/smoke.star"),
        Some("buildifier/tests/smoke.star"),
    ));
    assert!(
        near_miss.contains("`script:` is repository-root-relative")
            && near_miss.contains("write buildifier/tests/smoke.star"),
        "the near miss must name the path that would have worked, got: {near_miss}"
    );
}

#[test]
fn a_per_platform_test_script_is_checked_too() {
    // `platforms.<key>.tests` overrides the top-level list and is the only
    // list a container leg ever runs — nothing else validates it at all.
    render_with_script(
        &platform_script_spec("buildifier/tests/smoke.star"),
        Some("buildifier/tests/smoke.star"),
    )
    .expect("a per-platform script that exists must render");

    let missing = only_spec_error(render_with_script(
        &platform_script_spec("buildifier/tests/smoke.star"),
        None,
    ));
    assert!(
        missing.contains("platforms: 'linux/amd64': tests: entry 'smoke' script not found"),
        "the message must name the platform whose override it is, got: {missing}"
    );
}

#[test]
fn a_base_outside_the_repository_root_is_rejected() {
    // `paths:` names files of the workflow's own repository, so a trigger
    // for an out-of-root base is one that can never fire — the same silent
    // failure as an out-of-root spec, one step further out.
    let outer = tempdir().unwrap();
    let repo = outer.path().join("repo");
    write_file(outer.path(), "shared/base.yml", EXTENDS_BASE);
    let child = write_file(
        &repo,
        "buildifier/mirror.yml",
        &extends_child("buildifier", Some("../../shared/base.yml")),
    );

    match generate(&repo, &[child], false) {
        Err(MirrorError::SpecUsageError(msg)) => {
            assert!(
                msg.contains("base.yml") && msg.contains("--repo-root"),
                "the error must name the base and the fix, got: {msg}"
            );
        }
        other => panic!("a base outside the root must be a usage error, got: {other:?}"),
    }
    assert!(
        !repo.join(".github/workflows").exists(),
        "nothing may be written when the spec set is rejected"
    );
}

#[test]
fn the_drift_guard_records_every_spec_the_repository_has() {
    // `--spec` appends, so a guard naming a subset would re-render only that
    // subset and call the rest green. The committed guard is the record of
    // what the repository mirrors.
    let dir = tempdir().unwrap();
    generate(dir.path(), &two_spec_repo(dir.path()), false).unwrap();
    let guard = std::fs::read_to_string(dir.path().join(".github/workflows/verify-generated.yml")).unwrap();

    assert!(
        guard.contains("ocx-mirror package pipeline generate ci --check --spec mirror.yml --spec py3.13/mirror.yml\n"),
        "the guard must re-render every spec, got:\n{guard}"
    );
    assert!(
        guard.contains(
            "    paths:\n      - mirror.yml\n      - scripts/**\n      - tests/**\n      \
             - metadata*.json\n      - py3.13/**\n      - .github/workflows/**\n"
        ),
        "the guard's triggers must be the union of the specs', got:\n{guard}"
    );
}

#[test]
fn hand_editing_a_nested_workflow_reds_the_check_by_name() {
    let dir = tempdir().unwrap();
    let specs = two_spec_repo(dir.path());
    generate(dir.path(), &specs, false).unwrap();

    let edited = dir.path().join(".github/workflows/mirror-py3.13.yml");
    let mut content = std::fs::read_to_string(&edited).unwrap();
    content.push_str("\n# hand edit\n");
    std::fs::write(&edited, content).unwrap();

    match generate(dir.path(), &specs, true) {
        Err(MirrorError::RendererDrift(paths)) => {
            assert_eq!(
                paths,
                vec![".github/workflows/mirror-py3.13.yml"],
                "only the hand-edited file may be reported"
            );
        }
        other => panic!("a hand-edited nested workflow must red, got: {other:?}"),
    }
}

#[test]
fn dropping_a_spec_leaves_its_workflows_stale() {
    // Without the stale sweep the dropped spec's `mirror-py3.13.yml` keeps
    // running on schedule against a spec that no longer exists, and the
    // drift guard — which only ever compared files it renders — stays green.
    let dir = tempdir().unwrap();
    let specs = two_spec_repo(dir.path());
    generate(dir.path(), &specs, false).unwrap();

    // Hand-written workflows have no generated header and must be ignored.
    std::fs::write(dir.path().join(".github/workflows/release.yml"), "name: release\n").unwrap();

    match generate(dir.path(), &specs[..1], true) {
        Err(MirrorError::RendererDrift(paths)) => {
            assert_eq!(
                paths,
                vec![
                    // The committed guard still names the dropped spec —
                    // that is exactly how the repository records what it
                    // mirrors, so it drifts too.
                    ".github/workflows/verify-generated.yml",
                    ".github/workflows/announce-from-registry-py3.13.yml",
                    ".github/workflows/cascade-py3.13.yml",
                    ".github/workflows/describe-py3.13.yml",
                    ".github/workflows/mirror-py3.13.yml",
                    ".github/workflows/patch-py3.13.yml",
                ],
                "every workflow of the dropped spec is stale — and nothing else"
            );
        }
        other => panic!("dropping a spec must red on its leftover workflows, got: {other:?}"),
    }
}

#[test]
fn two_specs_in_one_directory_are_rejected() {
    // Names derive from the directory, so these two would overwrite each
    // other — silently, which is the whole failure being fixed.
    let dir = tempdir().unwrap();
    let first = install_spec("mirror-minimal.yml", dir.path());
    let second = install_spec_at("mirror-ghcr-announce.yml", dir.path(), "other.yml");

    match generate(dir.path(), &[first, second], false) {
        Err(MirrorError::SpecUsageError(msg)) => {
            assert!(msg.contains("mirror.yml") && msg.contains("other.yml"), "got: {msg}");
        }
        other => panic!("two specs in one directory must be a usage error, got: {other:?}"),
    }
    assert!(
        !dir.path().join(".github/workflows").exists(),
        "nothing may be written when the spec set is rejected"
    );
}

#[test]
fn a_spec_outside_the_repository_root_is_rejected() {
    let repo = tempdir().unwrap();
    let elsewhere = tempdir().unwrap();
    let outside = install_spec("mirror-minimal.yml", elsewhere.path());

    match generate(repo.path(), &[outside], false) {
        Err(MirrorError::SpecUsageError(msg)) => {
            assert!(msg.contains("--repo-root"), "the error must name the fix, got: {msg}");
        }
        other => panic!("a spec outside the root must be a usage error, got: {other:?}"),
    }
}

#[test]
fn the_drift_guard_survives_one_spec_opting_out() {
    // One guard covers the whole repository, so the opt-out only takes
    // effect when every spec asks for it — otherwise a single
    // `allow_manual_edits` would disarm the guard for its siblings too.
    let opt_out = format!("{SHFMT_SPEC}allow_manual_edits: true\n");
    let nested = slot_at("py3.13/mirror.yml");

    let mixed = render(&[
        (root_slot(), spec_from_yaml(SHFMT_SPEC)),
        (nested.clone(), spec_from_yaml(&opt_out)),
    ]);
    assert!(
        mixed.contains_key(Path::new(".github/workflows/verify-generated.yml")),
        "one spec still wanting the guard is enough to emit it"
    );

    let all_out = render(&[
        (root_slot(), spec_from_yaml(&opt_out)),
        (nested, spec_from_yaml(&opt_out)),
    ]);
    assert!(
        !all_out.contains_key(Path::new(".github/workflows/verify-generated.yml")),
        "the guard is dropped only when every spec opts out"
    );
}

#[test]
fn workflow_names_derive_from_the_spec_directory() {
    for (relative, suffix) in [
        ("mirror.yml", ""),
        ("py3.13/mirror.yml", "-py3.13"),
        ("a/b/mirror.yml", "-a-b"),
    ] {
        let slot = slot_at(relative);
        assert_eq!(slot.suffix(), suffix, "suffix for {relative}");
        assert_eq!(
            slot.workflow("mirror"),
            PathBuf::from(format!(".github/workflows/mirror{suffix}.yml")),
            "workflow path for {relative}"
        );
    }
}

/// Run `generate ci` with the repository root left to inference.
fn generate_inferring_root(specs: &[PathBuf], check: bool) -> Result<(), MirrorError> {
    let cmd = GenerateCi {
        spec: specs.to_vec(),
        repo_root: None,
        check,
        format: None,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    rt.block_on(async { cmd.execute(&printer).await })
}

/// A repository holding `count` package specs one level down, each with a
/// repo-root-relative `tests: script:` — and, with `with_base`, an
/// `extends:` base *above* the spec directory.
///
/// The two are separate fixtures because they fail separately and the
/// `extends:` check runs first: with a base present its error masks the
/// doubled `script:` path entirely, so a fix that only repaired `extends:`
/// would look green.
fn nested_spec_repo(root: &Path, count: usize, with_base: bool) -> Vec<PathBuf> {
    std::fs::create_dir_all(root.join(".git")).expect("mark the repository root");
    if with_base {
        write_file(root, "mirror-base.yml", EXTENDS_BASE);
    }
    (0..count)
        .map(|i| {
            let name = format!("tool{i}");
            write_file(root, &format!("{name}/tests/smoke.star"), "ocx_assert(True)\n");
            let base = with_base.then_some("../mirror-base.yml");
            let spec = extends_child(&name, base).replace(
                "  - name: version\n",
                &format!("  - name: smoke\n    script: {name}/tests/smoke.star\n  - name: version\n"),
            );
            write_file(root, &format!("{name}/mirror.yml"), &spec)
        })
        .collect()
}

/// A single spec one level down must infer the *repository* root, not its
/// own directory.
///
/// Repro (`mirror-astral-sh`): `tests: script:` is documented and
/// implemented as repo-root-relative, so an inferred root of `<repo>/tool0`
/// resolved it as `<repo>/tool0/tool0/tests/smoke.star` — doubled segment —
/// and the `extends:` base above the spec read as outside the repository.
/// Every single-spec-in-a-subdirectory repo failed its own
/// `verify-generated` drift guard. Multi-spec repos passed only because
/// their common ancestor happened to be the real root, which is why this
/// asserts both counts: one spec and three must infer the same root.
#[test]
fn a_nested_spec_infers_the_repository_root_whatever_the_spec_count() {
    // `(1, false)` is the `tests: script:` doubling on its own; `(1, true)`
    // adds the `extends:` base; `(3, true)` is the multi-spec repo that
    // used to pass by luck and must keep passing.
    for (count, with_base) in [(1, false), (1, true), (3, true)] {
        let case = format!("{count} spec(s), base above: {with_base}");
        let dir = tempdir().unwrap();
        let specs = nested_spec_repo(dir.path(), count, with_base);

        generate_inferring_root(&specs, false).unwrap_or_else(|e| panic!("{case} must render: {e}"));

        for i in 0..count {
            assert!(
                dir.path()
                    .join(format!(".github/workflows/mirror-tool{i}.yml"))
                    .exists(),
                "workflows must land at the repository root, not under the spec directory ({case})",
            );
        }
        // The generated guard has to pass against the same inference the
        // repository will run it with — the symptom was a repo that could
        // never satisfy its own drift check.
        generate_inferring_root(&specs, true).unwrap_or_else(|e| panic!("the drift guard must pass for {case}: {e}"));
    }
}

#[test]
fn the_repo_root_defaults_to_the_directory_the_specs_share() {
    // `generate ci --spec /elsewhere/repo/mirror.yml` has to write into that
    // repository. Defaulting the root to the process's own directory would
    // scatter generated workflows wherever the command happened to run.
    let dir = tempdir().unwrap();
    // Marked explicitly: without it the answer depends on whether TMPDIR
    // happens to sit inside a git repository, which is the difference
    // between exercising the git lookup and exercising the fallback.
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let spec = install_spec("mirror-minimal.yml", dir.path());
    let cmd = GenerateCi {
        spec: vec![spec],
        repo_root: None,
        check: false,
        format: None,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    rt.block_on(async { cmd.execute(&printer).await }).unwrap();

    assert!(
        dir.path().join(".github/workflows/mirror.yml").exists(),
        "the workflows must land next to the spec"
    );
}
