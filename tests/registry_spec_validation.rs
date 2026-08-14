// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The `registry.yml` validation corpus: one fixture per rejected document.
//!
//! Each `tests/fixtures/invalid_registry/*.yml` is a spec that must parse but
//! fail validation, with the diagnostics it must produce declared as leading
//! `# expect:` comments — one per substring, each of which must appear in at
//! least one reported error. The sibling of `tests/spec_validation.rs`, same
//! shape, different root type.
//!
//! **Fixtures carry no `kind:`.** Documents here go straight to serde, and
//! `RegistrySpec` has no `kind` field (C-001) — it is read by the pre-scan and
//! stripped by `load_registry_spec` before deserialization (C-007). A fixture
//! supplying one would fail to *parse*, which this harness treats as a broken
//! fixture rather than as the rejection under test.
//!
//! **The exit-64 class has no fixtures at all, deliberately.** Every pre-scan
//! rejection — credentials, a wrong or absent `kind:`, userinfo in a source
//! `index:` — fires *before* deserialization by construction, so it can never
//! reach `validate()`, and the `panic!` below would fire on each one. Those
//! live as unit tests in `src/spec/prescan/tests/`, which can additionally
//! assert the exit code and the absence of the offending value — neither of
//! which a fixture can express.

use std::path::{Path, PathBuf};

use ocx_mirror::spec::RegistrySpec;

fn fixture_dir() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/invalid_registry")).to_path_buf()
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
fn every_invalid_registry_fixture_reports_the_diagnostics_it_declares() {
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

        let spec: RegistrySpec = serde_yaml_ng::from_str(&source)
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
