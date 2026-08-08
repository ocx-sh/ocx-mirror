// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── §3.5 S5: ocx-mirror package pipeline plan — unit tests ────────────────────
//
// These tests verify the JSON output schema of PlanReport and the types
// involved. The actual plan computation (source/registry queries) is
// exercised via integration tests once execute() is implemented.

#[test]
fn plan_report_serializes_schema_version_3() {
    // §3.5: JSON output format matches design spec §2.2 schema.
    // schema_version 3 since the plan carries the metadata-drift kind and
    // the has_drift gate beside has_new.
    let report = PlanReport {
        schema_version: 3,
        has_new: true,
        has_drift: false,
        versions: vec![entry("3.29.0", &["linux/amd64", "darwin/arm64"], PlanVersionKind::New)],
        target: "ocx.sh/cmake".to_string(),
        ocx_mirror_rev: Some("abc123def456".to_string()),
    };

    let value: serde_json::Value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["schema_version"].as_u64().unwrap(), 3);
    assert!(value["has_new"].as_bool().unwrap());
    assert!(!value["has_drift"].as_bool().unwrap());
    assert_eq!(value["target"].as_str().unwrap(), "ocx.sh/cmake");
    assert_eq!(value["ocx_mirror_rev"].as_str().unwrap(), "abc123def456");
}

#[test]
fn plan_report_has_new_false_when_no_versions() {
    // §3.5: Empty source + empty target → has_new: false, versions: []
    let report = PlanReport {
        schema_version: 3,
        has_new: false,
        has_drift: false,
        versions: vec![],
        target: "ocx.sh/cmake".to_string(),
        ocx_mirror_rev: None,
    };

    let value: serde_json::Value = serde_json::to_value(&report).unwrap();
    assert!(!value["has_new"].as_bool().unwrap());
    assert!(value["versions"].as_array().unwrap().is_empty());
    // ocx_mirror_rev: null when None (serde default with Option)
}

#[test]
fn plan_version_kind_new_serializes_as_kebab_case() {
    // §3.5: PlanVersionKind::New → "new" in JSON (kebab-case)
    let value: serde_json::Value =
        serde_json::to_value(entry("3.29.0", &["linux/amd64"], PlanVersionKind::New)).unwrap();
    assert_eq!(value["kind"].as_str().unwrap(), "new");
}

#[test]
fn plan_version_kind_backfill_partial_serializes_as_kebab_case() {
    // §3.5: PlanVersionKind::BackfillPartial → "backfill-partial" in JSON
    let value: serde_json::Value =
        serde_json::to_value(entry("3.28.5", &["linux/arm64"], PlanVersionKind::BackfillPartial)).unwrap();
    assert_eq!(value["kind"].as_str().unwrap(), "backfill-partial");
}

#[test]
fn plan_report_mixed_new_and_backfill_versions() {
    // §3.5: Mixed: 2 versions present in target, 1 new → only 1 in versions[]
    // This test verifies the schema shape for the mixed case.
    let report = PlanReport {
        schema_version: 3,
        has_new: true,
        has_drift: false,
        versions: vec![
            entry("3.29.0", &["linux/amd64", "linux/arm64"], PlanVersionKind::New),
            entry("3.28.5", &["linux/arm64"], PlanVersionKind::BackfillPartial),
        ],
        target: "ocx.sh/cmake".to_string(),
        ocx_mirror_rev: None,
    };

    let value: serde_json::Value = serde_json::to_value(&report).unwrap();
    let versions = value["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["kind"].as_str().unwrap(), "new");
    assert_eq!(versions[1]["kind"].as_str().unwrap(), "backfill-partial");
    // Partial backfill: only missing platforms listed
    let partial_platforms = versions[1]["platforms"].as_array().unwrap();
    assert_eq!(partial_platforms.len(), 1);
    assert_eq!(partial_platforms[0].as_str().unwrap(), "linux/arm64");
}

#[test]
fn build_version_entries_emits_variant_prefixed_tag() {
    // Regression: a non-default variant must carry its own variant-prefixed
    // normalized tag in the plan. Both default + slim resolve to the same
    // bare upstream version (`3.13.9`); before the fix the plan emitted that
    // bare version for both, so `slim-3.13.9` never became its own matrix
    // leg and was never prepared, tested, or pushed by the workflow.
    use crate::filter::ResolvedVersion;
    use crate::resolver::asset_resolution::ResolvedPlatformAsset;

    let platform: Platform = "linux/amd64".parse().unwrap();
    let asset = || ResolvedPlatformAsset {
        platform: platform.clone(),
        asset_name: "cpython.tar.gz".to_string(),
        url: url::Url::parse("https://example.com/cpython.tar.gz").unwrap(),
    };

    let filtered = vec![
        ResolvedVersion {
            version: "3.13.9".to_string(),
            normalized_version: "3.13.9".to_string(),
            variant: None,
            platforms: vec![asset()],
            is_prerelease: false,
        },
        ResolvedVersion {
            version: "3.13.9".to_string(),
            normalized_version: "slim-3.13.9".to_string(),
            variant: Some("slim".to_string()),
            platforms: vec![asset()],
            is_prerelease: false,
        },
    ];

    let entries = build_version_entries(&filtered, &[], 0);
    let tags: Vec<&str> = entries.iter().map(|e| e.version.as_str()).collect();
    assert_eq!(
        tags,
        vec!["3.13.9", "slim-3.13.9"],
        "plan must emit the variant-prefixed normalized tag, not the bare upstream version"
    );
}

#[test]
fn build_version_entries_carries_resolved_assets() {
    // Regression (issue #160): plan entries must carry the resolved
    // per-platform assets (source_version, variant, asset URLs) so
    // `prepare --plan` consumes the discover crawl instead of re-running
    // the source generator once per matrix leg (N+1 crawls → GraphQL
    // rate-limit exhaustion).
    use crate::filter::ResolvedVersion;
    use crate::resolver::asset_resolution::ResolvedPlatformAsset;

    let platform: Platform = "linux/amd64".parse().unwrap();
    let filtered = vec![ResolvedVersion {
        version: "3.13.9".to_string(),
        normalized_version: "slim-3.13.9_20260610".to_string(),
        variant: Some("slim".to_string()),
        platforms: vec![ResolvedPlatformAsset {
            platform: platform.clone(),
            asset_name: "cpython-slim.tar.gz".to_string(),
            url: url::Url::parse("https://example.com/cpython-slim.tar.gz").unwrap(),
        }],
        is_prerelease: false,
    }];

    let entries = build_version_entries(&filtered, &[], 0);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.source_version, "3.13.9");
    assert_eq!(entry.variant.as_deref(), Some("slim"));
    assert_eq!(entry.assets.len(), 1);
    assert_eq!(entry.assets[0].platform, "linux/amd64");
    assert_eq!(entry.assets[0].asset_name, "cpython-slim.tar.gz");
    assert_eq!(entry.assets[0].url.as_str(), "https://example.com/cpython-slim.tar.gz");

    // Round-trip: prepare deserializes what plan serialized.
    let json = serde_json::to_string(&PlanReport {
        schema_version: 3,
        has_new: true,
        has_drift: false,
        versions: entries,
        target: "ocx.sh/cpython".to_string(),
        ocx_mirror_rev: None,
    })
    .unwrap();
    let parsed: PlanReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.versions[0].assets[0].asset_name, "cpython-slim.tar.gz");
}

#[test]
fn plan_cmd_execute_returns_ok_or_err_not_panic() {
    // §3.5: After implementation, execute() must not panic — it must return
    // a Result (Ok or Err). The prior stub-verification assertion (is_err on
    // catch_unwind) is now inverted: catch_unwind succeeds (is_ok) because
    // execute() no longer calls unimplemented!().
    //
    // When the spec file is absent, execute() returns Err(MirrorError::SourceError)
    // with exit code Unavailable — no panic.
    use std::panic;

    let cmd = PlanCmd {
        spec: std::path::PathBuf::from("./nonexistent-mirror.yml"),
        format: None,
        locks_dir: None,
    };
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rt.block_on(async { cmd.execute(&printer).await });
    }));
    // The closure must NOT panic — catch_unwind returns Ok.
    assert!(
        result.is_ok(),
        "PlanCmd::execute must not panic after implementation; got panic instead of Result"
    );
}
