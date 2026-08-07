// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── §TestEntry union: parse + validation ─────────────────────────────────

#[test]
fn parse_test_entry_command_kind() {
    // Happy path: `command:` field → TestKind::Command
    let yaml = r#"name: version
command: shfmt --version
"#;
    let entry: TestEntry = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(entry.name, "version");
    assert_eq!(entry.command.as_deref(), Some("shfmt --version"));
    assert!(entry.script.is_none());
    assert!(entry.script_inline.is_none());
    let kind = entry.kind().unwrap();
    assert_eq!(kind, TestKind::Command("shfmt --version"));
}

#[test]
fn parse_test_entry_script_kind() {
    // Happy path: `script:` field → TestKind::Script
    let yaml = r#"name: smoke
script: tests/smoke.star
"#;
    let entry: TestEntry = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(entry.command.is_none());
    assert_eq!(
        entry.script.as_ref().map(|p| p.to_str().unwrap()),
        Some("tests/smoke.star")
    );
    assert!(entry.script_inline.is_none());
    let kind = entry.kind().unwrap();
    assert!(matches!(kind, TestKind::Script(_)), "expected Script, got {kind:?}");
}

#[test]
fn parse_test_entry_script_inline_kind() {
    // Happy path: `script_inline:` field → TestKind::ScriptInline
    let yaml = "name: inline\nscript_inline: |\n  ocx_assert(True)\n";
    let entry: TestEntry = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(entry.command.is_none());
    assert!(entry.script.is_none());
    assert!(entry.script_inline.is_some());
    let kind = entry.kind().unwrap();
    assert!(
        matches!(kind, TestKind::ScriptInline(_)),
        "expected ScriptInline, got {kind:?}"
    );
}

#[test]
fn validate_test_entry_none_set_produces_error() {
    // Reject: no kind field set at all.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    let relevant: Vec<_> = errors.iter().filter(|e| e.contains("none set")).collect();
    assert!(
        !relevant.is_empty(),
        "Expected 'none set' error for entry with no kind, got: {errors:?}"
    );
    assert!(
        relevant[0].contains("version"),
        "Error must mention the entry name 'version': {relevant:?}"
    );
}

#[test]
fn validate_test_entry_multiple_set_produces_error() {
    // Reject: two kind fields set simultaneously.
    let yaml = format!(
        r#"{base}
tests:
  - name: multi
    command: shfmt --version
    script: tests/smoke.star
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    let relevant: Vec<_> = errors.iter().filter(|e| e.contains("set")).collect();
    assert!(
        !relevant.is_empty(),
        "Expected 'N set' error for entry with two kinds, got: {errors:?}"
    );
    assert!(
        relevant[0].contains("multi"),
        "Error must mention the entry name 'multi': {relevant:?}"
    );
    // Message must include a count (not zero)
    assert!(relevant[0].contains("2 set"), "Error must state '2 set': {relevant:?}");
}

#[test]
fn validate_test_entry_exactly_one_passes() {
    // Happy path through validate(): single command entry should not add
    // any kind-related errors.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    let kind_errors: Vec<_> = errors
        .iter()
        .filter(|e| e.contains("command|script|script_inline"))
        .collect();
    assert!(
        kind_errors.is_empty(),
        "Single-command entry must not produce kind errors: {errors:?}"
    );
}
