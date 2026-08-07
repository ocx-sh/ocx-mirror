// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── notify.discord.user_id ─────────────────────────────────────────────

#[test]
fn validate_accepts_valid_discord_user_id() {
    let yaml = format!(
        r#"{base}
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "123456789012345678"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        !errors.iter().any(|e| e.contains("user_id")),
        "valid snowflake must not error: {errors:?}"
    );
}

#[test]
fn validate_rejects_non_numeric_discord_user_id() {
    let yaml = format!(
        r#"{base}
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "12345"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("user_id") && e.contains("valid Discord user ID")),
        "short snowflake must error: {errors:?}"
    );
}

#[test]
fn policy_check_rejects_user_id_url_and_at_mention() {
    for (user_id, label) in [("https://discord.com/users/1", "URL"), ("@maintainer", "@mention")] {
        let yaml = format!(
            r#"{base}
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "{user_id}"
"#,
            base = MINIMAL_BASE_YAML
        );
        let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        let result = policy_check_notify(spec.notify.as_ref().unwrap());
        assert!(
            matches!(result, Err(MirrorError::SpecUsageError(_))),
            "user_id {label} must be a usage error: {result:?}"
        );
    }
}

#[test]
fn validate_r3_discord_com_in_webhook_secret() {
    // §3.1 R3 mitigation: webhook_secret containing "discord.com" → rejected
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: "https://discord.com/api/webhooks/1234/token"
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("webhook_secret") || e.contains("discord") || e.contains("URL")),
        "Expected R3 rejection for discord.com URL in webhook_secret, got: {errors:?}"
    );
}

#[test]
fn validate_r3_discordapp_com_in_webhook_secret() {
    // §3.1 R3 mitigation: webhook_secret containing "discordapp.com" → rejected
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: "https://discordapp.com/api/webhooks/1234/token"
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("webhook_secret") || e.contains("discordapp") || e.contains("URL")),
        "Expected R3 rejection for discordapp.com URL in webhook_secret, got: {errors:?}"
    );
}

#[test]
fn validate_r3_http_url_in_webhook_secret() {
    // §3.1 R3 mitigation: webhook_secret matching ^https?:// → rejected
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: "https://example.com/webhook/abc123"
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("webhook_secret") || e.contains("https") || e.contains("URL")),
        "Expected R3 rejection for http:// URL in webhook_secret, got: {errors:?}"
    );
}

#[test]
fn validate_r3_valid_secret_name_accepted() {
    // §3.1 R3 positive: valid GHA secret name accepted without error
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
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    // No webhook_secret errors expected
    let webhook_errors: Vec<_> = errors
        .iter()
        .filter(|e| e.contains("webhook_secret") || e.contains("discord"))
        .collect();
    assert!(
        webhook_errors.is_empty(),
        "Unexpected webhook_secret errors for valid GHA secret name: {webhook_errors:?}"
    );
}

#[test]
fn annotations_block_parses_and_defaults_to_empty() {
    let bare: MirrorSpec = serde_yaml_ng::from_str(MINIMAL_BASE_YAML).unwrap();
    assert!(bare.annotations.is_empty());

    let yaml = format!(
        r#"{base}
annotations:
  org.opencontainers.image.licenses: Apache-2.0
  org.opencontainers.image.source: https://github.com/upstream/project
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    assert_eq!(spec.annotations["org.opencontainers.image.licenses"], "Apache-2.0");
    assert_eq!(
        spec.annotations["org.opencontainers.image.source"],
        "https://github.com/upstream/project"
    );
    assert!(spec.validate(Path::new("test.yml")).is_empty());
}

#[test]
fn validate_rejects_annotation_key_containing_equals() {
    // `KEY=VALUE` is the wire form; a `=` in the key would re-split wrong
    // and publish a key the spec never asked for.
    let yaml = format!(
        r#"{base}
annotations:
  "bad=key": value
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("bad=key")),
        "expected rejection of '=' in annotation key, got: {errors:?}"
    );
}

#[test]
fn validate_per_platform_tests_override_replaces_top_level() {
    // §3.1: Per-platform tests: override replaces top-level entirely (no merge)
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
  - name: smoke
    command: bash ./tests/smoke.sh
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
  windows/amd64:
    runner: windows-latest
    shell: pwsh
    tests:
      - name: version
        command: shfmt.exe --version
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let platforms = spec.platforms.as_ref().unwrap();

    // Top-level tests: 2 entries
    let top_tests = spec.tests.as_ref().unwrap();
    assert_eq!(top_tests.len(), 2);

    // windows/amd64 override: 1 entry only (replacement, not merge)
    let windows = &platforms["windows/amd64"];
    let win_tests = windows.tests.as_ref().unwrap();
    assert_eq!(
        win_tests.len(),
        1,
        "Per-platform override must replace, not merge top-level tests"
    );
    assert_eq!(win_tests[0].name, "version");

    // linux/amd64 has no override — platforms[].tests is None
    let linux = &platforms["linux/amd64"];
    assert!(
        linux.tests.is_none(),
        "linux/amd64 must inherit top-level tests (no override)"
    );
}

#[test]
fn validate_default_shell_alpine_infers_sh() {
    // §3.1: Default-from-image shell inference: alpine:3.20 → sh
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    // alpine:3.20 has a known default (sh) — no ambiguous shell error expected
    let shell_errors: Vec<_> = errors
        .iter()
        .filter(|e| e.contains("shell") || e.contains("ambiguous"))
        .collect();
    assert!(
        shell_errors.is_empty(),
        "alpine:3.20 should have inferred shell 'sh'; got errors: {shell_errors:?}"
    );
}

#[test]
fn validate_default_shell_ubuntu_infers_bash() {
    // §3.1: Default-from-image shell inference: ubuntu:24.04 → bash
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
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    let shell_errors: Vec<_> = errors
        .iter()
        .filter(|e| e.contains("shell") || e.contains("ambiguous"))
        .collect();
    assert!(
        shell_errors.is_empty(),
        "ubuntu:24.04 should have inferred shell 'bash'; got errors: {shell_errors:?}"
    );
}
