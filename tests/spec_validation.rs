// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The spec-validation corpus: one fixture per rejected document.
//!
//! Each `tests/fixtures/invalid/*.yml` is a spec that must parse but fail
//! validation, with the diagnostics it must produce declared as leading
//! `# expect:` comments — one per substring, each of which must appear in at
//! least one reported error.
//!
//! Adding a validation rule means adding a fixture, not another near-copy of
//! the same four lines. Tests that inspect parsed fields, assert a document is
//! *valid*, or go through `load_spec`'s `extends:` chain stay as Rust beside
//! the code — a fixture loop cannot express those.

use std::path::{Path, PathBuf};

use ocx_mirror::spec::MirrorSpec;

fn fixture_dir() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/invalid")).to_path_buf()
}

/// The `# expect:` substrings declared at the top of a fixture.
fn expectations(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| line.strip_prefix("# expect:"))
        .map(|rest| rest.trim().to_string())
        .collect()
}

#[test]
fn every_invalid_fixture_reports_the_diagnostics_it_declares() {
    let dir = fixture_dir();
    let mut checked = 0;

    for entry in std::fs::read_dir(&dir).expect("fixture directory is readable") {
        let path = entry.expect("readable directory entry").path();
        if path.extension().is_none_or(|ext| ext != "yml") {
            continue;
        }
        let name = path
            .file_name()
            .expect("fixture has a name")
            .to_string_lossy()
            .to_string();
        let source = std::fs::read_to_string(&path).expect("fixture is readable");

        let expected = expectations(&source);
        assert!(
            !expected.is_empty(),
            "{name}: fixture declares no `# expect:` line, so it asserts nothing"
        );

        let spec: MirrorSpec = serde_yaml_ng::from_str(&source)
            .unwrap_or_else(|e| panic!("{name}: fixture must parse — validation is what it tests: {e}"));
        let errors = spec.validate(Path::new(&name));

        for want in &expected {
            assert!(
                errors.iter().any(|e| e.contains(want)),
                "{name}: expected an error containing {want:?}, got: {errors:?}"
            );
        }
        checked += 1;
    }

    assert!(checked > 0, "no fixtures found under {}", dir.display());
}
