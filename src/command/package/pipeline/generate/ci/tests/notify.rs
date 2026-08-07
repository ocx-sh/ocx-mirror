// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── Discord user-id env injection ─────────────────────────────────────────

const NOTIFY_SPEC_WITH_USER_ID: &str = r#"
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
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "123456789012345678"
"#;

#[test]
fn render_injects_discord_user_id_into_notify_env() {
    let spec = spec_from_yaml(NOTIFY_SPEC_WITH_USER_ID);
    let workflow = render_workflow(&spec, &root_slot());
    assert!(
        workflow.contains("OCX_MIRROR_DISCORD_USER_ID: \"123456789012345678\""),
        "notify env must inline the configured user id; workflow:\n{workflow}"
    );
    // The hook secret line and the user-id line both live in the notify env.
    assert!(workflow.contains("OCX_MIRROR_DISCORD_HOOK: ${{ secrets.DISCORD_WEBHOOK_URL }}"));
}

#[test]
fn render_omits_discord_user_id_when_unset() {
    let spec = spec_from_yaml(SHFMT_SPEC);
    let workflow = render_workflow(&spec, &root_slot());
    assert!(
        !workflow.contains("OCX_MIRROR_DISCORD_USER_ID"),
        "no user-id env line when user_id is unset"
    );
    assert!(
        !workflow.contains("{DISCORD_USER_ID_ENV}"),
        "the user-id placeholder must always be substituted"
    );
}
