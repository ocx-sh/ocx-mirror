// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::discord::colors;
use crate::run_summary::ExcludedPlatform;

// ── Message-construction tests (no HTTP, no env var needed) ────────────

#[test]
fn notify_silent_when_all_skipped_existing() {
    // §3.9: all skipped_existing + no test_failures → silent (exit 0, no POST).
    let _guard = WebhookEnvGuard::unset();
    let result = run_notify_sync(&make_all_skipped_summary());
    assert!(
        matches!(result, Ok(())),
        "all-skipped summary must be silent (exit 0, no POST): {result:?}"
    );
}

#[test]
fn notify_missing_env_var_returns_spec_usage_error() {
    // OCX_MIRROR_DISCORD_HOOK unset → SpecUsageError (exit 64) when a POST is due.
    let _guard = WebhookEnvGuard::unset();
    let result = run_notify_sync(&make_all_green_summary());
    assert!(
        matches!(result, Err(MirrorError::SpecUsageError(_))),
        "unset webhook env var must return SpecUsageError: {result:?}"
    );
}

#[test]
fn green_version_embed_has_green_color_and_title() {
    let messages = build_messages(&make_all_green_summary(), None);
    let embed = only_embed(&messages);
    assert_eq!(embed.color, colors::GREEN);
    assert_eq!(embed.title, "ocx.sh/shfmt: 3.7.0 published");
    // No mention: all green.
    assert!(messages[0].content.is_none());
    assert!(messages[0].allowed_mentions.is_none());
}

#[test]
fn partial_version_embed_has_yellow_color_and_title() {
    let messages = build_messages(&make_partial_summary(), None);
    let embed = only_embed(&messages);
    assert_eq!(embed.color, colors::YELLOW);
    assert_eq!(embed.title, "ocx.sh/shfmt: 3.7.0 partial");
}

#[test]
fn failed_version_embed_has_red_color_and_title() {
    let messages = build_messages(&make_all_failed_summary(), None);
    let embed = only_embed(&messages);
    assert_eq!(embed.color, colors::RED);
    assert_eq!(embed.title, "ocx.sh/shfmt: 3.7.0 failed");
}

#[test]
fn title_falls_back_to_mirror_when_target_empty() {
    let mut summary = make_all_green_summary();
    summary.target = String::new();
    let messages = build_messages(&summary, None);
    assert_eq!(only_embed(&messages).title, "shfmt: 3.7.0 published");
}

#[test]
fn embed_has_exactly_two_inline_columns() {
    let messages = build_messages(&make_partial_summary(), None);
    let embed = only_embed(&messages);
    let names: Vec<&str> = embed.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["Platform", "Status"]);
    for f in &embed.fields {
        assert!(f.inline, "every column must be inline: {f:?}");
    }
}

#[test]
fn green_embed_lists_each_platform_with_chip() {
    let messages = build_messages(&make_all_green_summary(), None);
    let embed = only_embed(&messages);
    assert_eq!(col_platform(embed).value, "`linux/amd64`\n`darwin/arm64`");
    assert_eq!(col_status(embed).value, "`🟢`\n`🟢`");
}

#[test]
fn green_status_cell_links_to_push_job_url() {
    let mut summary = make_all_green_summary();
    summary.push_job_url = Some("https://github.com/ocx-sh/mirror-shfmt/actions/runs/2/job/3".to_string());
    let messages = build_messages(&summary, None);
    let embed = only_embed(&messages);
    let row = "[`🟢`](https://github.com/ocx-sh/mirror-shfmt/actions/runs/2/job/3)";
    assert_eq!(col_status(embed).value, format!("{row}\n{row}"));
}

#[test]
fn failed_status_cell_wraps_chip_in_link_when_job_url_present() {
    let mut summary = make_all_failed_summary();
    let job_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/7";
    summary.versions[0].platforms_failed[0].job_url = Some(job_url.to_string());
    let messages = build_messages(&summary, None);
    let embed = only_embed(&messages);
    assert_eq!(col_status(embed).value, format!("[`🔴`]({job_url})"));
}

#[test]
fn missing_bundle_reason_uses_no_entry_glyph() {
    let mut summary = make_all_failed_summary();
    summary.versions[0].platforms_failed[0].reason = "missing_bundle".to_string();
    let messages = build_messages(&summary, None);
    assert_eq!(col_status(only_embed(&messages)).value, "`🚫`");
}

// ── 🔒 excluded-platform rows ──────────────────────────────────────────

#[test]
fn excluded_platform_renders_lock_row_with_reason() {
    let mut summary = make_all_green_summary();
    summary.versions[0].platforms_excluded = vec![ExcludedPlatform {
        platform: "windows/arm64".to_string(),
        reason: Some("aarch64-windows build-exe segfault".to_string()),
    }];
    let messages = build_messages(&summary, None);
    let embed = only_embed(&messages);
    assert!(
        col_platform(embed).value.contains("`windows/arm64`"),
        "excluded platform must appear in the Platform column: {}",
        col_platform(embed).value,
    );
    assert!(
        col_status(embed)
            .value
            .contains("`🔒` aarch64-windows build-exe segfault"),
        "🔒 row must carry the reason: {}",
        col_status(embed).value,
    );
}

#[test]
fn excluded_platform_without_reason_renders_bare_lock() {
    let mut summary = make_all_green_summary();
    summary.versions[0].platforms_excluded = vec![ExcludedPlatform {
        platform: "windows/arm64".to_string(),
        reason: None,
    }];
    let messages = build_messages(&summary, None);
    let status = &col_status(only_embed(&messages)).value;
    assert!(status.contains("`🔒`"), "bare 🔒 chip expected: {status}");
    assert!(status.ends_with("`🔒`"), "no trailing reason text: {status}");
}

// ── In-message mention ─────────────────────────────────────────────────

#[test]
fn no_mention_when_user_id_absent_even_on_failure() {
    let messages = build_messages(&make_partial_summary(), None);
    assert!(messages[0].content.is_none(), "no content without a user id");
    assert!(messages[0].allowed_mentions.is_none());
}

#[test]
fn no_mention_when_all_green_even_with_user_id() {
    let messages = build_messages(&make_all_green_summary(), Some("123456789012345678"));
    assert!(
        messages[0].content.is_none(),
        "all-green message must not ping: {:?}",
        messages[0].content
    );
    assert!(messages[0].allowed_mentions.is_none());
}

#[test]
fn partial_message_pings_user_scoped() {
    let id = "123456789012345678";
    let messages = build_messages(&make_partial_summary(), Some(id));
    assert_eq!(messages[0].content.as_deref(), Some("<@123456789012345678>"));
    let allowed = messages[0].allowed_mentions.as_ref().expect("ping must scope mentions");
    assert!(allowed.parse.is_empty(), "parse must be empty so only the user pings");
    assert_eq!(
        allowed.users.as_deref(),
        Some(["123456789012345678".to_string()].as_slice())
    );
}

#[test]
fn failed_message_pings_user() {
    let messages = build_messages(&make_all_failed_summary(), Some("123456789012345678"));
    assert_eq!(messages[0].content.as_deref(), Some("<@123456789012345678>"));
    assert!(messages[0].allowed_mentions.is_some());
}
