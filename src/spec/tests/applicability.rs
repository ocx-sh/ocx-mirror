// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── Per-platform version applicability ─────────────────────────────────

/// A spec exercising every applicability lever: an undeclared platform
/// (linux/amd64), a late-introduced platform with a broken single exclude
/// (windows/arm64), and a dropped platform with an open-ended skip range
/// (darwin/amd64).
fn spec_with_platform_windows() -> MirrorSpec {
    let yaml = format!(
        r#"{base}
platforms:
  linux/amd64:
    runner: ubuntu-latest
  windows/arm64:
    runner: windows-11-arm
    min_version: "0.11.7"
    exclude:
      - version: "0.16.0"
        reason: "aarch64-windows build-exe segfault"
        severity: broken
  darwin/amd64:
    runner: macos-14
    max_version: "11.1.0"
    exclude:
      - max_version: "9.4.0"
        severity: skip
"#,
        base = MINIMAL_BASE_YAML
    );
    serde_yaml_ng::from_str(&yaml).expect("applicability spec must parse")
}

#[test]
fn validate_accepts_platform_applicability_window() {
    let spec = spec_with_platform_windows();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(errors.is_empty(), "valid applicability spec must not error: {errors:?}");
}

#[test]
fn platform_applies_respects_min_inclusive() {
    let spec = spec_with_platform_windows();
    assert!(
        !spec.platform_applies("0.11.6", "windows/arm64"),
        "below min is dropped"
    );
    assert!(spec.platform_applies("0.11.7", "windows/arm64"), "min is inclusive");
    assert!(spec.platform_applies("0.12.0", "windows/arm64"));
}

#[test]
fn platform_applies_respects_max_exclusive() {
    let spec = spec_with_platform_windows();
    assert!(spec.platform_applies("11.0.0", "darwin/amd64"));
    assert!(!spec.platform_applies("11.1.0", "darwin/amd64"), "max is exclusive");
    assert!(!spec.platform_applies("12.0.0", "darwin/amd64"));
}

#[test]
fn platform_applies_drops_single_and_range_excludes() {
    let spec = spec_with_platform_windows();
    assert!(
        !spec.platform_applies("0.16.0", "windows/arm64"),
        "single exclude dropped"
    );
    assert!(spec.platform_applies("0.17.0", "windows/arm64"), "outside exclude kept");
    // darwin/amd64 open-ended `max_version: 9.4.0` skip range.
    assert!(!spec.platform_applies("9.3.0", "darwin/amd64"), "range exclude dropped");
    assert!(spec.platform_applies("9.4.0", "darwin/amd64"), "range max is exclusive");
}

#[test]
fn platform_applies_true_for_undeclared_or_unconstrained_platform() {
    let spec = spec_with_platform_windows();
    // Declared but no bounds/excludes.
    assert!(spec.platform_applies("0.1.0", "linux/amd64"));
    // Not declared in `platforms:` at all.
    assert!(spec.platform_applies("0.1.0", "linux/arm64"));
}

#[test]
fn platform_applies_strips_build_metadata() {
    let spec = spec_with_platform_windows();
    // A build-stamped run version compares on its release core.
    assert!(!spec.platform_applies("0.16.0_20260604120000", "windows/arm64"));
    assert!(spec.platform_applies("0.17.0_20260604120000", "windows/arm64"));
}

#[test]
fn exclude_hit_reports_matching_entry_with_severity_and_reason() {
    let spec = spec_with_platform_windows();
    let hit = spec.exclude_hit("0.16.0", "windows/arm64").expect("0.16.0 is excluded");
    assert_eq!(hit.severity, Severity::Broken);
    assert_eq!(hit.reason.as_deref(), Some("aarch64-windows build-exe segfault"));

    // Build-stamped version still resolves to the entry.
    assert!(spec.exclude_hit("0.16.0_20260604", "windows/arm64").is_some());

    let skip = spec.exclude_hit("9.3.0", "darwin/amd64").expect("9.3.0 is excluded");
    assert_eq!(skip.severity, Severity::Skip);

    assert!(
        spec.exclude_hit("0.17.0", "windows/arm64").is_none(),
        "non-excluded → None"
    );
    assert!(
        spec.exclude_hit("0.16.0", "linux/amd64").is_none(),
        "platform has no excludes"
    );
}

#[test]
fn platform_applies_ignores_variant_prefix() {
    let spec = spec_with_platform_windows();
    // Variant mirrors (e.g. cpython `debug`/`pgo.lto`) key off variant-prefixed
    // version strings. Applicability compares on the release core regardless.
    assert!(
        !spec.platform_applies("debug-0.16.0", "windows/arm64"),
        "single exclude dropped under variant"
    );
    assert!(
        !spec.platform_applies("debug-0.11.6", "windows/arm64"),
        "below min dropped under variant"
    );
    assert!(
        spec.platform_applies("debug-0.11.7", "windows/arm64"),
        "min inclusive under variant"
    );
    // darwin/amd64 open-ended range exclude `max_version: 9.4.0`.
    assert!(
        !spec.platform_applies("debug-9.3.0", "darwin/amd64"),
        "range exclude dropped under variant"
    );
    // Variant + build stamp together.
    assert!(!spec.platform_applies("debug-0.16.0_20260604120000", "windows/arm64"));
}

#[test]
fn exclude_hit_matches_variant_prefixed_version() {
    let spec = spec_with_platform_windows();
    // Single-version exclude branch.
    let hit = spec
        .exclude_hit("debug-0.16.0", "windows/arm64")
        .expect("variant version resolves single exclude");
    assert_eq!(hit.severity, Severity::Broken);
    assert!(spec.exclude_hit("debug-0.16.0_20260604", "windows/arm64").is_some());
    // Range exclude branch (darwin/amd64 open-ended max 9.4.0, skip).
    let skip = spec
        .exclude_hit("debug-9.3.0", "darwin/amd64")
        .expect("variant version in range exclude");
    assert_eq!(skip.severity, Severity::Skip);
}

#[test]
fn validate_rejects_unparseable_platform_bounds() {
    let yaml = format!(
        r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    min_version: "not-a-version"
    max_version: "also bad"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("min_version") && e.contains("not a valid version")),
        "bad min_version must error: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("max_version") && e.contains("not a valid version")),
        "bad max_version must error: {errors:?}"
    );
}

#[test]
fn validate_rejects_exclude_with_version_and_range() {
    let yaml = format!(
        r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - version: "1.0.0"
        max_version: "2.0.0"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("exclude[0]") && e.contains("cannot set both")),
        "version + range must error: {errors:?}"
    );
}

#[test]
fn validate_rejects_empty_exclude_entry() {
    let yaml = format!(
        r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - reason: "no bounds at all"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("exclude[0]") && e.contains("must set")),
        "empty exclude entry must error: {errors:?}"
    );
}

#[test]
fn validate_rejects_invalid_exclude_version() {
    let yaml = format!(
        r#"{base}
platforms:
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - version: "garbage"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("exclude[0]") && e.contains("not a valid version")),
        "unparseable exclude version must error: {errors:?}"
    );
}
