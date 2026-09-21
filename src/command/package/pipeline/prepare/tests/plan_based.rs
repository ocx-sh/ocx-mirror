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
        legs: Default::default(),
        versions_resolved: Default::default(),
    }
}

fn asset_entry(platform: &str, name: &str) -> PlanAssetEntry {
    PlanAssetEntry {
        platform: platform.to_string(),
        asset_name: name.to_string(),
        url: url::Url::parse(&format!("https://example.com/{name}")).unwrap(),
        digest: None,
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

    let tasks = build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "1.2.3").unwrap();

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

    let err = build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "9.9.9").unwrap_err();
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
    let mut plan = plan_with(vec![PlanVersionEntry {
        version: "1.2.3".to_string(),
        platforms: vec!["linux/amd64".to_string()],
        kind: PlanVersionKind::New,
        source_version: String::new(),
        variant: None,
        assets: vec![],
        pylock: None,
    }]);
    plan.schema_version = 1;

    let err = build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "1.2.3").unwrap_err();
    match err {
        MirrorError::PlanError(msg) => {
            assert!(msg.contains("no resolved assets"), "unexpected message: {msg}");
            assert!(
                msg.contains("schema_version >= 2"),
                "a genuinely old plan must still get the regenerate advice: {msg}"
            );
        }
        other => panic!("expected PlanError, got {other:?}"),
    }
}

// ── issue #85: a drift entry is not an old plan ─────────────────────────

#[test]
fn build_tasks_from_plan_names_patch_for_a_metadata_drift_entry() {
    // `plan` emits a drift entry with `assets: []` by construction, and
    // `prepare` used to blame the plan's schema version for it — advice the
    // reader cannot follow, since the plan was written by the same binary
    // seconds earlier. Name the verb that does repair it.
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let mut plan = plan_with(vec![PlanVersionEntry {
        version: "2.1.267".to_string(),
        platforms: vec!["linux/amd64".to_string()],
        kind: PlanVersionKind::MetadataDrift,
        // Empty by construction: a normalized tag cannot be reversed into the
        // upstream version it was stamped from.
        source_version: String::new(),
        variant: None,
        assets: vec![],
        pylock: None,
    }]);
    plan.has_new = false;
    plan.has_drift = true;

    match build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "2.1.267").unwrap_err() {
        MirrorError::PlanError(msg) => {
            assert!(
                msg.contains("patch --spec mirror.yml --metadata-only --version 2.1.267"),
                "the message must carry the command that repairs it, `--spec` included — \
                 `patch` defaults it to ./mirror.yml, which in a multi-spec repository either \
                 misses or patches a different package: {msg}"
            );
            assert!(
                !msg.contains("schema_version"),
                "a drift entry is not a schema problem: {msg}"
            );
        }
        other => panic!("expected PlanError, got {other:?}"),
    }
}

#[test]
fn build_tasks_from_plan_does_not_blame_the_schema_on_a_current_plan() {
    // A hand-edited current-schema entry is neither drift nor an old plan.
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let plan = plan_with(vec![PlanVersionEntry {
        version: "1.2.3".to_string(),
        platforms: vec!["linux/amd64".to_string()],
        kind: PlanVersionKind::New,
        source_version: "1.2.3".to_string(),
        variant: None,
        assets: vec![],
        pylock: None,
    }]);

    match build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "1.2.3").unwrap_err() {
        MirrorError::PlanError(msg) => {
            assert!(
                !msg.contains("schema_version >= 2"),
                "schema_version 2 advice must not fire on a schema_version 2 plan: {msg}"
            );
            assert!(msg.contains("hand-edited"), "unexpected message: {msg}");
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

    let err = build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "slim-1.2.3").unwrap_err();
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

    let tasks = build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "0.10.0").unwrap();
    assert_eq!(platforms_of(&tasks), vec!["linux/amd64".to_string()]);
}

// ── issue #83: `--version` means one thing, not two ─────────────────────

#[test]
fn build_tasks_from_plan_accepts_the_bare_source_version() {
    // The renderer's case: `plan.json` carries a stamped `version` next to the
    // bare `source_version`, nothing says which one `prepare --version` takes,
    // and passing the bare one used to fail only under `--plan`. Both forms
    // now resolve to the same entry, and the tasks carry the stamped tag.
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let plan = plan_with(vec![PlanVersionEntry {
        version: "1.2.3_20260920084410".to_string(),
        platforms: vec!["linux/amd64".to_string()],
        kind: PlanVersionKind::New,
        source_version: "1.2.3".to_string(),
        variant: None,
        assets: vec![asset_entry("linux/amd64", "tool-linux-amd64")],
        pylock: None,
    }]);

    for requested in ["1.2.3", "1.2.3_20260920084410"] {
        let tasks = build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, requested).unwrap();
        assert_eq!(tasks.len(), 1, "{requested} must resolve to the one entry");
        assert_eq!(
            tasks[0].normalized_version, "1.2.3_20260920084410",
            "{requested} must publish under the plan's own tag"
        );
    }
}

#[test]
fn build_tasks_from_plan_refuses_an_ambiguous_bare_version() {
    // A variant spec stamps two tags from one upstream release, so the bare
    // version names both. Taking the first would prepare the wrong artifact
    // without a word.
    let spec: MirrorSpec = serde_yaml_ng::from_str(UNREACHABLE_SOURCE_SPEC).unwrap();
    let entry = |version: &str, variant: Option<&str>| PlanVersionEntry {
        version: version.to_string(),
        platforms: vec!["linux/amd64".to_string()],
        kind: PlanVersionKind::New,
        source_version: "1.2.3".to_string(),
        variant: variant.map(str::to_string),
        assets: vec![asset_entry("linux/amd64", "tool-linux-amd64")],
        pylock: None,
    };
    let plan = plan_with(vec![
        entry("1.2.3_20260920084410", None),
        entry("slim-1.2.3_20260920084410", Some("slim")),
    ]);

    match build_tasks_from_plan(&spec, Path::new("."), Path::new("mirror.yml"), &plan, "1.2.3").unwrap_err() {
        MirrorError::PlanError(message) => {
            assert!(
                message.contains("names 2 plan entries"),
                "unexpected message: {message}"
            );
            assert!(
                message.contains("slim-1.2.3_20260920084410"),
                "must name the candidates: {message}"
            );
        }
        other => panic!("expected PlanError, got {other:?}"),
    }

    // An exact tag stays unambiguous even though the bare form is not.
    let tasks = build_tasks_from_plan(
        &spec,
        Path::new("."),
        Path::new("mirror.yml"),
        &plan,
        "1.2.3_20260920084410",
    )
    .unwrap();
    assert_eq!(tasks[0].normalized_version, "1.2.3_20260920084410");
}
