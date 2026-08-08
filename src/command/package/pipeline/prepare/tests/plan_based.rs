// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::command::package::pipeline::plan::{PlanAssetEntry, PlanVersionEntry, PlanVersionKind};
use std::panic;
use std::path::Path;

// ── issue #160: plan-based task building (no source re-crawl) ───────────

/// Spec whose source is unreachable by construction (unroutable remote
/// url_index). Any code path that queries the source fails; plan-based
/// task building must succeed regardless.
const UNREACHABLE_SOURCE_SPEC: &str = r#"
name: testtool
target:
  registry: ocx.sh
  repository: testtool
source:
  type: url_index
  url: "http://127.0.0.1:1/index.json"
assets:
  linux/amd64:
    - "tool-linux-amd64$"
  darwin/arm64:
    - "tool-darwin-arm64$"
asset_type:
  type: binary
  name: tool
build_timestamp: none
"#;

fn plan_with(versions: Vec<PlanVersionEntry>) -> PlanReport {
    PlanReport {
        schema_version: 2,
        has_new: !versions.is_empty(),
        has_drift: false,
        versions,
        target: "ocx.sh/testtool".to_string(),
        ocx_mirror_rev: None,
    }
}

fn asset_entry(platform: &str, name: &str) -> PlanAssetEntry {
    PlanAssetEntry {
        platform: platform.to_string(),
        asset_name: name.to_string(),
        url: url::Url::parse(&format!("https://example.com/{name}")).unwrap(),
    }
}

#[test]
fn build_tasks_from_plan_does_not_query_source() {
    // Regression (issue #160): N prepare matrix legs re-crawling the
    // source exhausted the GitHub GraphQL points budget. With --plan,
    // tasks come from the plan's resolved assets — the (unreachable)
    // source is never queried, so this must succeed offline.
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let plan = plan_with(vec![PlanVersionEntry {
        version: "1.2.3".to_string(),
        platforms: vec!["linux/amd64".to_string(), "darwin/arm64".to_string()],
        kind: PlanVersionKind::New,
        source_version: "1.2.3".to_string(),
        variant: None,
        assets: vec![
            asset_entry("linux/amd64", "tool-linux-amd64"),
            asset_entry("darwin/arm64", "tool-darwin-arm64"),
        ],
        pylock: None,
    }]);

    let tasks = build_tasks_from_plan(&spec, Path::new("."), &plan, "1.2.3").unwrap();

    assert_eq!(tasks.len(), 2);
    let task = tasks.iter().find(|t| t.platform.to_string() == "linux/amd64").unwrap();
    assert_eq!(task.version, "1.2.3");
    assert_eq!(task.normalized_version, "1.2.3");
    assert_eq!(task.asset_name, "tool-linux-amd64");
    assert_eq!(task.download_url.as_str(), "https://example.com/tool-linux-amd64");
    assert!(task.variant.is_none());
}

#[test]
fn build_tasks_from_plan_errors_on_missing_version() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let plan = plan_with(vec![]);

    let err = build_tasks_from_plan(&spec, Path::new("."), &plan, "9.9.9").unwrap_err();
    assert!(
        matches!(err, MirrorError::PlanError(_)),
        "expected PlanError, got {err:?}"
    );
}

#[test]
fn build_tasks_from_plan_errors_on_plan_without_assets() {
    // A schema_version-1 plan parses (serde defaults) but carries no
    // resolved assets — prepare must fail with an actionable error
    // instead of silently building nothing.
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let plan = plan_with(vec![PlanVersionEntry {
        version: "1.2.3".to_string(),
        platforms: vec!["linux/amd64".to_string()],
        kind: PlanVersionKind::New,
        source_version: String::new(),
        variant: None,
        assets: vec![],
        pylock: None,
    }]);

    let err = build_tasks_from_plan(&spec, Path::new("."), &plan, "1.2.3").unwrap_err();
    match err {
        MirrorError::PlanError(msg) => {
            assert!(msg.contains("no resolved assets"), "unexpected message: {msg}");
        }
        other => panic!("expected PlanError, got {other:?}"),
    }
}

#[test]
fn build_tasks_from_plan_errors_on_unknown_variant() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let plan = plan_with(vec![PlanVersionEntry {
        version: "slim-1.2.3".to_string(),
        platforms: vec!["linux/amd64".to_string()],
        kind: PlanVersionKind::New,
        source_version: "1.2.3".to_string(),
        variant: Some("slim".to_string()),
        assets: vec![asset_entry("linux/amd64", "tool-linux-amd64")],
        pylock: None,
    }]);

    let err = build_tasks_from_plan(&spec, Path::new("."), &plan, "slim-1.2.3").unwrap_err();
    assert!(
        matches!(err, MirrorError::PlanError(_)),
        "expected PlanError, got {err:?}"
    );
}

#[test]
fn build_tasks_from_plan_respects_platform_applicability() {
    // Same applicability rules as the crawl path: out-of-window pairs in a
    // (hand-edited) plan are dropped, not built.
    let spec: MirrorSpec = serde_yaml_ng::from_str(APPLICABILITY_SPEC).unwrap();
    let plan = plan_with(vec![PlanVersionEntry {
        version: "0.10.0".to_string(),
        platforms: vec!["linux/amd64".to_string(), "windows/arm64".to_string()],
        kind: PlanVersionKind::New,
        source_version: "0.10.0".to_string(),
        variant: None,
        assets: vec![
            asset_entry("linux/amd64", "tool-linux-amd64"),
            // Below windows/arm64's min_version (0.11.7) → must be dropped.
            asset_entry("windows/arm64", "tool-windows-arm64"),
        ],
        pylock: None,
    }]);

    let tasks = build_tasks_from_plan(&spec, Path::new("."), &plan, "0.10.0").unwrap();
    assert_eq!(platforms_of(&tasks), vec!["linux/amd64".to_string()]);
}
