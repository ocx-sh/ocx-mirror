// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use std::path::Path;
use tempfile::tempdir;

// ── Multi-spec repositories ───────────────────────────────────────────────

fn workflows_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(".github/workflows"))
        .expect("renderer must write .github/workflows")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn two_specs_render_two_workflow_sets_and_one_drift_guard() {
    // The bug repeatable `--spec` exists to fix: rendering the second spec
    // used to overwrite the first, because every filename was fixed.
    let dir = tempdir().unwrap();
    let specs = two_spec_repo(dir.path());
    generate(dir.path(), &specs, false).expect("two specs must render");

    assert_eq!(
        workflows_in(dir.path()),
        vec![
            "announce-from-registry-py3.13.yml",
            "cascade-py3.13.yml",
            "cascade.yml",
            "describe-py3.13.yml",
            "describe.yml",
            "mirror-py3.13.yml",
            "mirror.yml",
            "patch-py3.13.yml",
            "patch.yml",
            "verify-generated.yml",
        ],
        "each spec owns a workflow set named after its directory, and the \
         repository owns exactly one drift guard"
    );

    generate(dir.path(), &specs, true).expect("--check must be green right after a render");
}

#[test]
fn spec_argument_order_does_not_change_what_is_rendered() {
    // Output that depended on argument order would make the drift guard —
    // which passes the specs in one fixed order — red for anyone who typed
    // them differently.
    let forward = tempdir().unwrap();
    let reverse = tempdir().unwrap();
    let mut specs = two_spec_repo(forward.path());
    generate(forward.path(), &specs, false).unwrap();

    let mut reversed = two_spec_repo(reverse.path());
    reversed.reverse();
    generate(reverse.path(), &reversed, false).unwrap();

    specs.sort();
    for name in workflows_in(forward.path()) {
        let a = std::fs::read_to_string(forward.path().join(".github/workflows").join(&name)).unwrap();
        let b = std::fs::read_to_string(reverse.path().join(".github/workflows").join(&name)).unwrap();
        assert_eq!(a, b, "{name} differs when the specs are passed in the other order");
    }
}

#[test]
fn a_nested_spec_names_itself_in_every_generated_invocation() {
    // Without `--spec`, every pipeline command in the nested spec's
    // workflows would fall back to the repo-root `mirror.yml` and mirror
    // the wrong tool while looking perfectly green.
    let dir = tempdir().unwrap();
    generate(dir.path(), &two_spec_repo(dir.path()), false).unwrap();

    let read = |name: &str| std::fs::read_to_string(dir.path().join(".github/workflows").join(name)).unwrap();

    let mirror = read("mirror-py3.13.yml");
    for command in ["plan", "prepare", "push"] {
        assert!(
            mirror.contains(&format!("pipeline {command} --spec py3.13/mirror.yml")),
            "`pipeline {command}` must name its own spec, got:\n{mirror}"
        );
    }
    assert!(
        read("describe-py3.13.yml").contains("pipeline describe --spec py3.13/mirror.yml"),
        "describe must name its own spec"
    );
    assert!(
        read("announce-from-registry-py3.13.yml").contains("pipeline announce --spec py3.13/mirror.yml --dry-run"),
        "announce must name its own spec"
    );
    assert!(
        read("patch-py3.13.yml").contains("pipeline patch --spec py3.13/mirror.yml --metadata-only"),
        "patch must name its own spec"
    );
    assert!(
        read("cascade-py3.13.yml").contains("pipeline cascade --spec py3.13/mirror.yml --dry-run"),
        "cascade must name its own spec"
    );

    // The root spec is the one path `--spec` already defaults to, so it
    // stays unsaid — that is what keeps the published corpus byte-identical.
    let root = read("mirror.yml");
    assert!(
        !root.contains("--spec"),
        "the repo-root spec must not name itself, got:\n{root}"
    );
}

#[test]
fn a_nested_spec_triggers_only_on_its_own_subtree() {
    // Repo-wide triggers would wake every spec's workflow on every commit —
    // forty mirror runs for a one-line change in one subdirectory.
    let dir = tempdir().unwrap();
    generate(dir.path(), &two_spec_repo(dir.path()), false).unwrap();

    let nested = std::fs::read_to_string(dir.path().join(".github/workflows/mirror-py3.13.yml")).unwrap();
    assert!(
        nested.contains("      - py3.13/**\n      - .github/workflows/mirror-py3.13.yml\n"),
        "a nested spec watches its own subtree and its own workflow, got:\n{nested}"
    );
    // `script:` resolves from the repo root while this trigger covers only
    // the subtree — the gap has to be stated where it bites.
    assert!(
        nested.contains("# `script:` paths resolve from the repository root, not from py3.13/"),
        "the subtree trigger must warn about repo-root-relative script paths, got:\n{nested}"
    );
    // That note is injected *inside* a YAML sequence, so a string assertion
    // cannot see whether it corrupted the sequence. Parsing can.
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&nested)
        .unwrap_or_else(|e| panic!("a nested spec must render parseable YAML: {e}\n{nested}"));
    let paths = parsed["on"]["push"]["paths"]
        .as_sequence()
        .unwrap_or_else(|| panic!("push.paths must survive as a sequence, got:\n{nested}"));
    assert_eq!(
        paths.iter().map(|p| p.as_str().unwrap()).collect::<Vec<_>>(),
        vec!["py3.13/**", ".github/workflows/mirror-py3.13.yml"],
        "the comment must not become an entry"
    );
    assert!(
        !nested.contains("- scripts/**"),
        "a nested spec must not claim the repository-wide paths, got:\n{nested}"
    );

    let describe = std::fs::read_to_string(dir.path().join(".github/workflows/describe-py3.13.yml")).unwrap();
    assert!(
        describe.contains("      - py3.13/**\n      - .github/workflows/describe-py3.13.yml\n"),
        "got:\n{describe}"
    );
    assert!(
        describe.contains("name: describe-py3.13\n"),
        "sibling describes need distinct workflow names — `concurrency.group` keys \
         on `github.workflow`, so identical names would serialise them, got:\n{describe}"
    );
}

// ── Shared `extends:` bases ───────────────────────────────────────────────

/// The `on.<event>.paths` sequence of a rendered workflow.
///
/// Parsed rather than string-matched: the entries share their block with a
/// generated comment, and only a parser can tell an entry from a note.
fn trigger_entries(workflow: &str, event: &str) -> Vec<String> {
    let parsed: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(workflow).unwrap_or_else(|e| panic!("a rendered workflow must parse: {e}\n{workflow}"));
    parsed["on"][event]["paths"]
        .as_sequence()
        .unwrap_or_else(|| panic!("on.{event}.paths must be a sequence, got:\n{workflow}"))
        .iter()
        .map(|entry| entry.as_str().expect("path entries are strings").to_string())
        .collect()
}

#[test]
fn a_spec_that_extends_a_base_triggers_on_that_base() {
    // The shared base sits above every child's subtree, so the subtree
    // trigger cannot see it: editing the platform matrix there changed what
    // every package publishes and re-ran nothing.
    let workflows_for = |extends: Option<&str>| -> (String, String) {
        let dir = tempdir().unwrap();
        write_file(dir.path(), "mirror-base.yml", EXTENDS_BASE);
        let child = write_file(
            dir.path(),
            "buildifier/mirror.yml",
            &extends_child("buildifier", extends),
        );
        generate(dir.path(), &[child], false).expect("the child spec must render");
        let read = |name: &str| std::fs::read_to_string(dir.path().join(".github/workflows").join(name)).unwrap();
        (read("mirror-buildifier.yml"), read("describe-buildifier.yml"))
    };

    let (mirror, describe) = workflows_for(Some("../mirror-base.yml"));
    assert_eq!(
        trigger_entries(&mirror, "push"),
        vec![
            "buildifier/**",
            "mirror-base.yml",
            ".github/workflows/mirror-buildifier.yml"
        ],
        "a spec watches its own subtree, every base it extends, and its own workflow, got:\n{mirror}"
    );
    assert_eq!(
        trigger_entries(&describe, "push"),
        vec![
            "buildifier/**",
            "mirror-base.yml",
            ".github/workflows/describe-buildifier.yml"
        ],
        "the base decides the target registry describe publishes to, got:\n{describe}"
    );

    // Red proof: the same package with the base's keys inlined instead of
    // extended. Without this the assertion above would pass for a renderer
    // that names `mirror-base.yml` unconditionally.
    let (standalone, _) = workflows_for(None);
    assert_eq!(
        trigger_entries(&standalone, "push"),
        vec!["buildifier/**", ".github/workflows/mirror-buildifier.yml"],
        "a spec that extends nothing watches nothing extra, got:\n{standalone}"
    );
}

#[test]
fn a_base_inside_the_specs_own_subtree_adds_no_entry() {
    // The subtree glob already covers a base under the spec's own directory.
    // The sibling case is what keeps that check honest: `buildifier-extra/`
    // shares a prefix with `buildifier/` and is not under it.
    for (base_at, base_ref, expected) in [
        ("buildifier/base.yml", "./base.yml", vec!["buildifier/**"]),
        (
            "buildifier-extra/base.yml",
            "../buildifier-extra/base.yml",
            vec!["buildifier/**", "buildifier-extra/base.yml"],
        ),
    ] {
        let dir = tempdir().unwrap();
        write_file(dir.path(), base_at, EXTENDS_BASE);
        let child = write_file(
            dir.path(),
            "buildifier/mirror.yml",
            &extends_child("buildifier", Some(base_ref)),
        );
        generate(dir.path(), &[child], false).expect("the child spec must render");
        let workflow = std::fs::read_to_string(dir.path().join(".github/workflows/mirror-buildifier.yml")).unwrap();

        let mut expected = expected;
        expected.push(".github/workflows/mirror-buildifier.yml");
        assert_eq!(
            trigger_entries(&workflow, "push"),
            expected,
            "trigger for a spec extending {base_ref}, got:\n{workflow}"
        );
    }
}

#[test]
fn the_drift_guard_watches_a_shared_base_once() {
    // The guard re-renders every spec, so a base edit changes every
    // generated workflow in the repository — the one change the guard was
    // blind to. Listing it once per child would be equally correct to GHA
    // and unreadable in the committed file.
    let dir = tempdir().unwrap();
    write_file(dir.path(), "mirror-base.yml", EXTENDS_BASE);
    let specs: Vec<PathBuf> = ["buildifier", "buildozer"]
        .iter()
        .map(|name| {
            write_file(
                dir.path(),
                &format!("{name}/mirror.yml"),
                &extends_child(name, Some("../mirror-base.yml")),
            )
        })
        .collect();
    generate(dir.path(), &specs, false).expect("two specs sharing a base must render");

    let guard = std::fs::read_to_string(dir.path().join(".github/workflows/verify-generated.yml")).unwrap();
    for event in ["pull_request", "push"] {
        assert_eq!(
            trigger_entries(&guard, event),
            vec![
                "buildifier/**",
                "mirror-base.yml",
                "buildozer/**",
                ".github/workflows/**"
            ],
            "the guard's {event} trigger must cover the shared base exactly once, got:\n{guard}"
        );
    }
}
