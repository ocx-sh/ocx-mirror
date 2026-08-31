// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use std::path::Path;
use tempfile::tempdir;

// ── §3.3 S3: Golden tests for ocx-mirror generate ci ──────────────────

#[test]
fn render_minimal_spec_writes_workflow() {
    // §3.3: Fixture mirror-minimal.yml → renderer produces workflow YAML.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-minimal.yml", dir.path());
    match result {
        Ok(()) => {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
            assert!(workflow.exists(), "Expected .github/workflows/mirror.yml to be written");
            let content = std::fs::read_to_string(&workflow).unwrap();
            // Generated file must have the DO-NOT-EDIT header
            assert!(
                content.contains("DO NOT EDIT"),
                "Generated workflow must contain 'DO NOT EDIT' header"
            );
            // Must install ocx via the setup-ocx action (replaces the old
            // submodule + `cargo install` pair)
            assert!(
                content.contains("uses: ocx-sh/setup-ocx@"),
                "Generated workflow must install ocx via the setup-ocx action"
            );
            // Pipeline subcommands are invoked directly — setup-ocx has
            // already activated the project toolchain onto PATH for the step.
            assert!(
                content.contains("ocx-mirror package pipeline plan"),
                "Generated workflow must invoke ocx-mirror directly (no `ocx exec --` wrapper)"
            );
            // Lock the toolchain-sourcing model: no step wraps a tool in
            // `ocx exec --` (that would pin the bootstrap ocx, breaking the
            // nested `ocx package push` resolution). Both spellings are barred:
            // `run` is the pre-0.6 name and is deleted in 0.7, so a wrapper
            // could come back under either.
            for wrapper in ["ocx exec -- ", "ocx run -- "] {
                assert!(
                    !content.contains(wrapper),
                    "Generated workflow must not wrap tools in `{wrapper}`; content:\n{content}"
                );
            }
        }
        Err(MirrorError::SpecUsageError(_)) => {
            panic!("mirror-minimal.yml should be a valid spec, got SpecUsageError");
        }
        Err(e) => {
            panic!("Unexpected error rendering minimal fixture: {e}");
        }
    }
}

// ── Zero-drift guard for the native corpus ────────────────────────────────

/// Every fixture that renders successfully and declares no `containers:`.
///
/// These stand in for the ~40 pinned mirror repositories in the wild: their
/// generated workflows are committed and guarded by `verify-generated.yml`,
/// so any renderer change that shifts a single byte turns every one of them
/// red on its next run. Adding a fixture here is deliberate friction — a new
/// native fixture needs a new golden.
const NATIVE_FIXTURES: &[&str] = &[
    "mirror-minimal.yml",
    "mirror-full-platforms.yml",
    "mirror-ghcr-announce.yml",
    "mirror-generator-source.yml",
    "mirror-two-platform-announce.yml",
    "mirror-windows-arm64.yml",
    "mirror-all-test-kinds.yml",
    "mirror-variants.yml",
    "mirror-pylock.yml",
    "mirror-pypi.yml",
];

/// Render every generated file for `fixture` into one comparable blob,
/// with the build-stamped header values masked.
///
/// `VERSION` bumps each release and `GIT_SHA_SHORT` changes on every commit,
/// so both are replaced by fixed tokens — masking the stamps is what lets the
/// golden assert on the parts a renderer change can actually break.
fn render_all_masked(fixture: &str) -> String {
    let dir = tempdir().unwrap();
    render_fixture(fixture, dir.path()).unwrap_or_else(|e| panic!("{fixture} must render: {e}"));

    let workflows = dir.path().join(".github/workflows");
    let mut entries: Vec<_> = std::fs::read_dir(&workflows)
        .expect("renderer must write .github/workflows")
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();

    let mut blob = String::new();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let content = std::fs::read_to_string(&path).unwrap();
        blob.push_str(&format!("===== {name} =====\n"));
        blob.push_str(&content.replace(VERSION, "{VERSION}").replace(GIT_SHA_SHORT, "{REV}"));
    }
    blob
}

#[test]
fn native_specs_render_byte_identically_to_their_goldens() {
    // The single assertion that protects the pinned mirror corpus: a spec
    // without `containers:` must render exactly the bytes it renders today.
    // Regenerate deliberately with `UPDATE_GOLDEN=1 cargo test -p ocx-mirror`
    // and read the diff before committing it.
    let golden_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden"));
    for fixture in NATIVE_FIXTURES {
        let rendered = render_all_masked(fixture);
        let golden_path = golden_dir.join(format!("{}.txt", fixture.trim_end_matches(".yml")));

        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(golden_dir).unwrap();
            std::fs::write(&golden_path, &rendered).unwrap();
            continue;
        }

        let golden = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
            panic!(
                "missing golden {} for {fixture} ({e}); regenerate with UPDATE_GOLDEN=1",
                golden_path.display()
            )
        });
        assert_eq!(
            rendered, golden,
            "{fixture} drifted from its golden — every pinned mirror repo \
             rendering a native spec would see this change"
        );
    }
}
