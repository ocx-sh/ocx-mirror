// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::discord::colors;
use crate::run_summary::PlatformFailure;

// ── Per-version messages (C1: one message per version) ────────────────

#[test]
fn one_message_per_version() {
    // C1: 2 published versions → 2 messages, each with exactly 1 embed.
    // OLD behavior (now rejected): 2 versions → 1 message with 2 embeds.
    let versions = vec![
        {
            let mut v = version(VersionStatus::Published, "3.7.0");
            v.platforms_pushed = vec!["linux/amd64".to_string()];
            v
        },
        {
            let mut v = version(VersionStatus::Published, "3.8.0");
            v.platforms_pushed = vec!["linux/amd64".to_string()];
            v
        },
    ];
    let summary = run_summary(versions, true, false);
    let messages = build_messages(&summary, None);
    assert_eq!(messages.len(), 2, "2 published versions must produce 2 messages");
    assert_eq!(messages[0].embeds.len(), 1, "each message carries exactly 1 embed");
    assert_eq!(messages[1].embeds.len(), 1, "each message carries exactly 1 embed");
    assert_eq!(messages[0].embeds[0].title, "ocx.sh/shfmt: 3.7.0 published");
    assert_eq!(messages[1].embeds[0].title, "ocx.sh/shfmt: 3.8.0 published");
}

#[test]
fn skipped_existing_versions_produce_no_embed() {
    // C1: a skipped-existing version with no rows yields no message.
    // The one published version produces 1 message with 1 embed.
    let versions = vec![
        {
            let mut v = version(VersionStatus::Published, "3.8.0");
            v.platforms_pushed = vec!["linux/amd64".to_string()];
            v
        },
        version(VersionStatus::SkippedExisting, "3.7.0"),
    ];
    let summary = run_summary(versions, true, false);
    let messages = build_messages(&summary, None);
    assert_eq!(messages.len(), 1, "skipped-existing version yields no message");
    assert_eq!(messages[0].embeds.len(), 1, "the published version yields 1 embed");
    assert_eq!(messages[0].embeds[0].title, "ocx.sh/shfmt: 3.8.0 published");
}

#[test]
fn each_published_version_is_its_own_message() {
    // C1: 11 published versions → 11 messages, each with exactly 1 embed.
    // OLD behavior (now rejected): 11 versions → 2 messages (10+1 batching).
    let versions: Vec<VersionSummary> = (0..11)
        .map(|i| {
            let mut v = version(VersionStatus::Published, &format!("1.0.{i}"));
            v.platforms_pushed = vec!["linux/amd64".to_string()];
            v
        })
        .collect();
    let summary = run_summary(versions, true, false);
    let messages = build_messages(&summary, None);
    assert_eq!(messages.len(), 11, "11 versions must produce 11 messages, not 2");
    for (i, msg) in messages.iter().enumerate() {
        assert_eq!(msg.embeds.len(), 1, "message {i} must carry exactly 1 embed");
    }
    // Oldest-first order preserved: first message is 1.0.0, last is 1.0.10.
    assert!(
        messages[0].embeds[0].title.contains("1.0.0"),
        "first message must be for version 1.0.0: {}",
        messages[0].embeds[0].title
    );
    assert!(
        messages[10].embeds[0].title.contains("1.0.10"),
        "last message must be for version 1.0.10: {}",
        messages[10].embeds[0].title
    );
}

#[test]
fn only_failing_versions_message_carries_the_ping() {
    // C1 + ping scoping: 3 versions where the middle one is Failed.
    // → 3 messages; ONLY the failed version's message pings.
    // The two green messages must have content == None.
    let id = "123456789012345678";
    let mut published_v100 = version(VersionStatus::Published, "1.0.0");
    published_v100.platforms_pushed = vec!["linux/amd64".to_string()];
    let mut failed_v101 = version(VersionStatus::Failed, "1.0.1");
    failed_v101.platforms_failed = vec![PlatformFailure {
        platform: "linux/amd64".to_string(),
        reason: "test_failed".to_string(),
        failed_tests: vec![],
        job_url: None,
    }];
    let mut published_v102 = version(VersionStatus::Published, "1.0.2");
    published_v102.platforms_pushed = vec!["linux/amd64".to_string()];
    let summary = run_summary(vec![published_v100, failed_v101, published_v102], true, true);
    let messages = build_messages(&summary, Some(id));
    assert_eq!(messages.len(), 3, "3 versions must produce 3 messages");
    // Green version 1.0.0 → no ping.
    assert!(
        messages[0].content.is_none(),
        "green 1.0.0 message must not carry a ping: {:?}",
        messages[0].content
    );
    assert!(
        messages[0].allowed_mentions.is_none(),
        "green message must have no allowed_mentions"
    );
    // Failed version 1.0.1 → ping with scoped mentions.
    assert_eq!(
        messages[1].content.as_deref(),
        Some("<@123456789012345678>"),
        "failed 1.0.1 message must carry the ping"
    );
    assert!(
        messages[1].allowed_mentions.is_some(),
        "failed message must have scoped allowed_mentions"
    );
    // Green version 1.0.2 → no ping.
    assert!(
        messages[2].content.is_none(),
        "green 1.0.2 message must not carry a ping: {:?}",
        messages[2].content
    );
    assert!(
        messages[2].allowed_mentions.is_none(),
        "green message must have no allowed_mentions"
    );
}

#[test]
fn only_first_message_carries_author_and_thumbnail() {
    // C1 decoration rule: first *emitted* message's embed gets author + thumbnail;
    // later messages' embeds do not.
    // OLD test asserted first embed in a single message; now first of multiple messages.
    let mut summary = make_all_green_summary();
    summary.source_url = Some("https://github.com/mvdan/sh".to_string());
    summary.logo_url = Some("https://raw.githubusercontent.com/ocx-sh/mirror-shfmt/abc/logo.png".to_string());
    summary.versions.push({
        let mut v = version(VersionStatus::Published, "3.8.0");
        v.platforms_pushed = vec!["linux/amd64".to_string()];
        v
    });
    let messages = build_messages(&summary, None);
    assert_eq!(messages.len(), 2, "2 versions must produce 2 messages");
    // First message's embed is decorated.
    assert_eq!(messages[0].embeds.len(), 1);
    assert!(
        messages[0].embeds[0].author.is_some(),
        "first message's embed carries the author strip"
    );
    assert!(
        messages[0].embeds[0].thumbnail.is_some(),
        "first message's embed carries the thumbnail"
    );
    // Second message's embed is NOT decorated.
    assert_eq!(messages[1].embeds.len(), 1);
    assert!(
        messages[1].embeds[0].author.is_none(),
        "second message's embed must not carry author"
    );
    assert!(
        messages[1].embeds[0].thumbnail.is_none(),
        "second message's embed must not carry thumbnail"
    );
}

#[test]
fn first_emitted_embed_decorated_when_leading_version_yields_no_embed() {
    // Regression guard: versions stored oldest-first; versions[0] is often
    // a SkippedExisting that produces no message. The author strip + thumbnail
    // must decorate the first *emitted* message, not summary.versions[0].
    //
    // Under the new C1 contract: 1 skipped + 1 published → 1 message (from
    // the published version); its embed must carry author + thumbnail.
    let mut summary = make_all_green_summary();
    summary.source_url = Some("https://github.com/mvdan/sh".to_string());
    summary.logo_url = Some("https://raw.githubusercontent.com/ocx-sh/mirror-shfmt/abc/logo.png".to_string());
    // Prepend an older, already-mirrored version with no rows → no message.
    summary
        .versions
        .insert(0, version(VersionStatus::SkippedExisting, "3.6.0"));
    let messages = build_messages(&summary, None);
    assert_eq!(messages.len(), 1, "only the published version yields a message");
    assert_eq!(messages[0].embeds.len(), 1);
    assert!(
        messages[0].embeds[0].author.is_some(),
        "the first emitted message must carry the author strip"
    );
    assert!(
        messages[0].embeds[0].thumbnail.is_some(),
        "the first emitted message must carry the thumbnail"
    );
}

#[test]
fn each_message_has_correct_color_and_non_empty_url() {
    // C1 acceptance (a): 3 published versions with distinct statuses →
    // 3 messages; assert each message's single embed has the color matching
    // that version's status AND a non-empty url.
    let mut published_v100 = version(VersionStatus::Published, "1.0.0");
    published_v100.platforms_pushed = vec!["linux/amd64".to_string()];
    let mut partial_v101 = version(VersionStatus::Partial, "1.0.1");
    partial_v101.platforms_pushed = vec!["linux/amd64".to_string()];
    partial_v101.platforms_failed = vec![PlatformFailure {
        platform: "darwin/arm64".to_string(),
        reason: "test_failed".to_string(),
        failed_tests: vec![],
        job_url: None,
    }];
    let mut failed_v102 = version(VersionStatus::Failed, "1.0.2");
    failed_v102.platforms_failed = vec![PlatformFailure {
        platform: "linux/amd64".to_string(),
        reason: "test_failed".to_string(),
        failed_tests: vec![],
        job_url: None,
    }];
    let summary = run_summary(vec![published_v100, partial_v101, failed_v102], true, true);
    let messages = build_messages(&summary, None);
    assert_eq!(messages.len(), 3, "3 versions must produce 3 messages");
    // Message 0: Published → green.
    let embed0 = &messages[0].embeds[0];
    assert_eq!(embed0.color, colors::GREEN, "published version must be green");
    assert!(
        embed0.url.as_deref().is_some_and(|u| !u.is_empty()),
        "published version embed must have a non-empty url"
    );
    // Message 1: Partial → yellow.
    let embed1 = &messages[1].embeds[0];
    assert_eq!(embed1.color, colors::YELLOW, "partial version must be yellow");
    assert!(
        embed1.url.as_deref().is_some_and(|u| !u.is_empty()),
        "partial version embed must have a non-empty url"
    );
    // Message 2: Failed → red.
    let embed2 = &messages[2].embeds[0];
    assert_eq!(embed2.color, colors::RED, "failed version must be red");
    assert!(
        embed2.url.as_deref().is_some_and(|u| !u.is_empty()),
        "failed version embed must have a non-empty url"
    );
}

#[test]
fn build_author_renders_github_owner_and_repo_with_avatar() {
    let mut summary = make_all_green_summary();
    summary.source_url = Some("https://github.com/mvdan/sh".to_string());
    let author = build_author(&summary).expect("github source_url must yield author");
    assert_eq!(author.name, "mvdan/sh");
    assert_eq!(author.url.as_deref(), Some("https://github.com/mvdan/sh"));
    assert_eq!(author.icon_url.as_deref(), Some("https://github.com/mvdan.png?size=64"));
}

#[test]
fn build_author_uses_generic_label_for_non_github_url() {
    let mut summary = make_all_green_summary();
    summary.source_url = Some("https://example.org/project".to_string());
    let author = build_author(&summary).expect("non-empty source_url must yield author");
    assert_eq!(author.name, "View source");
    assert!(author.icon_url.is_none());
}

#[test]
fn build_thumbnail_omits_when_logo_url_unset() {
    assert!(build_thumbnail(None).is_none());
    assert!(build_thumbnail(Some("")).is_none());
    assert!(build_thumbnail(Some("   ")).is_none());
}

#[test]
fn webhook_env_var_name_is_conventional() {
    assert_eq!(WEBHOOK_ENV_VAR, "OCX_MIRROR_DISCORD_HOOK");
    assert_eq!(USER_ID_ENV_VAR, "OCX_MIRROR_DISCORD_USER_ID");
}
