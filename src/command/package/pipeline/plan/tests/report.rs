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
fn plan_report_serializes_schema_version_4() {
    // §3.5: JSON output format matches design spec §2.2 schema.
    // schema_version 4 since the plan carries a per-asset digest, the
    // per-platform legs map and the resolved version window, on top of v3's
    // metadata-drift kind and has_drift gate.
    let report = PlanReport {
        schema_version: PLAN_SCHEMA_VERSION,
        has_new: true,
        has_drift: false,
        versions: vec![entry("3.29.0", &["linux/amd64", "darwin/arm64"], PlanVersionKind::New)],
        target: "ocx.sh/cmake".to_string(),
        ocx_mirror_rev: Some("abc123def456".to_string()),
        legs: Default::default(),
        versions_resolved: Default::default(),
    };

    let value: serde_json::Value = serde_json::to_value(&report).unwrap();
    assert_eq!(
        value["schema_version"].as_u64().unwrap(),
        u64::from(PLAN_SCHEMA_VERSION)
    );
    assert!(value["has_new"].as_bool().unwrap());
    assert!(!value["has_drift"].as_bool().unwrap());
    assert_eq!(value["target"].as_str().unwrap(), "ocx.sh/cmake");
    assert_eq!(value["ocx_mirror_rev"].as_str().unwrap(), "abc123def456");
}

#[test]
fn plan_report_has_new_false_when_no_versions() {
    // §3.5: Empty source + empty target → has_new: false, versions: []
    let report = PlanReport {
        schema_version: PLAN_SCHEMA_VERSION,
        has_new: false,
        has_drift: false,
        versions: vec![],
        target: "ocx.sh/cmake".to_string(),
        ocx_mirror_rev: None,
        legs: Default::default(),
        versions_resolved: Default::default(),
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
        schema_version: PLAN_SCHEMA_VERSION,
        has_new: true,
        has_drift: false,
        versions: vec![
            entry("3.29.0", &["linux/amd64", "linux/arm64"], PlanVersionKind::New),
            entry("3.28.5", &["linux/arm64"], PlanVersionKind::BackfillPartial),
        ],
        target: "ocx.sh/cmake".to_string(),
        ocx_mirror_rev: None,
        legs: Default::default(),
        versions_resolved: Default::default(),
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
        digest: None,
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
            digest: None,
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
        schema_version: PLAN_SCHEMA_VERSION,
        has_new: true,
        has_drift: false,
        versions: entries,
        target: "ocx.sh/cpython".to_string(),
        ocx_mirror_rev: None,
        legs: Default::default(),
        versions_resolved: Default::default(),
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
    let printer = ocx_console::DataInterface::new(ocx_console::Printer::new(false, false));
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

// ── #77: the `legs` map ───────────────────────────────────────────────────

/// A spec with one container platform and one native platform, plus a
/// per-platform `tests:` override on the native one.
const LEGS_SPEC_YAML: &str = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64+libc.musl:
    - "shfmt_v.*_linux_amd64$"
  darwin/arm64:
    - "shfmt_v.*_darwin_arm64$"
asset_type:
  type: binary
  name: shfmt
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64+libc.musl:
    runner: [self-hosted, linux, x64]
    containers:
      - image: alpine:3.20
        setup:
          - apk add --no-cache libstdc++
  darwin/arm64:
    runner: macos-latest
    tests:
      - name: smoke
        script_inline: |
          ocx_assert(True)
"#;

#[test]
fn build_legs_resolves_the_spec_matrix_per_platform_key() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(LEGS_SPEC_YAML).expect("legs spec must parse");
    let legs = build_legs(&spec);

    assert_eq!(
        legs.keys().collect::<Vec<_>>(),
        vec!["darwin/arm64", "linux/amd64+libc.musl"],
        "keys are the `platforms:` keys verbatim, in the sorted order the renderer emits"
    );

    let container = &legs["linux/amd64+libc.musl"];
    assert_eq!(container.runner, vec!["self-hosted", "linux", "x64"]);
    // The slug `pipeline prepare` writes the bundle under — not `/` → `_`.
    assert_eq!(container.platform_slug, "linux_amd64_libc.musl");
    // What `docker run --platform` accepts: the libc suffix is stripped.
    assert_eq!(container.docker_platform, "linux/amd64");
    assert_eq!(container.containers.len(), 1);
    let image = &container.containers[0];
    assert_eq!(image.id, "alpine_3_20", "id defaults to the slugified image");
    assert_eq!(image.image.as_deref(), Some("alpine:3.20"));
    assert_eq!(image.shell, "sh", "inferred from the alpine basename");
    assert_eq!(image.libc.as_deref(), Some("musl"));
    assert_eq!(
        image.setup.as_deref(),
        Some(["apk add --no-cache libstdc++".to_owned()].as_slice())
    );
    // No per-platform override → the top-level list.
    assert_eq!(
        container.tests.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["version"]
    );

    let native = &legs["darwin/arm64"];
    assert_eq!(native.runner, vec!["macos-latest"], "a single label is still a list");
    assert_eq!(native.docker_platform, "darwin/arm64");
    assert_eq!(
        native.containers.len(),
        1,
        "a container-less platform gets the sentinel"
    );
    assert_eq!(native.containers[0].id, "_native_");
    assert_eq!(native.containers[0].shell, "bash");
    // The per-platform `tests:` REPLACES the top-level list, never merges.
    assert_eq!(
        native.tests.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["smoke"]
    );
}

#[test]
fn the_native_leg_omits_image_and_libc_on_the_wire() {
    // A consumer reads "absent" as native. Serialising `null` there would make
    // every renderer test for two things instead of one.
    let spec: MirrorSpec = serde_yaml_ng::from_str(LEGS_SPEC_YAML).expect("legs spec must parse");
    let report = PlanReport {
        schema_version: PLAN_SCHEMA_VERSION,
        has_new: false,
        has_drift: false,
        versions: vec![],
        target: "ocx.sh/shfmt".to_string(),
        ocx_mirror_rev: None,
        legs: build_legs(&spec),
        versions_resolved: Default::default(),
    };

    let json = serde_json::to_string(&report).expect("plan report serialises");
    let value: serde_json::Value = serde_json::from_str(&json).expect("and parses back");
    let native = &value["legs"]["darwin/arm64"]["containers"][0];
    for absent in ["image", "libc", "setup"] {
        assert!(
            native.get(absent).is_none(),
            "the native leg must omit `{absent}`, got: {native}"
        );
    }
    assert_eq!(
        value["legs"]["darwin/arm64"]["runner"],
        serde_json::json!(["macos-latest"])
    );

    let round_tripped: PlanReport = serde_json::from_str(&json).expect("plan report round trips");
    assert_eq!(
        serde_json::to_value(&round_tripped).unwrap(),
        value,
        "the legs survive a serialise → deserialise round trip unchanged"
    );
}

#[test]
fn build_legs_is_empty_without_a_platforms_block() {
    // An env or archive spec that never declares `platforms:` has no matrix,
    // and `{}` is what the contract promises there — not a missing key.
    let spec: MirrorSpec = serde_yaml_ng::from_str(
        &LEGS_SPEC_YAML[..LEGS_SPEC_YAML
            .find("platforms:")
            .expect("fixture has a platforms block")],
    )
    .expect("spec without platforms must parse");
    assert!(build_legs(&spec).is_empty());
}
