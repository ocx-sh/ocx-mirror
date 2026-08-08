// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── Index-announce row ─────────────────────────────────────────────────

fn announce_row(embed: &DiscordEmbed) -> &DiscordEmbedField {
    embed
        .fields
        .iter()
        .find(|f| f.name == "Index")
        .unwrap_or_else(|| panic!("embed carries no Index row: {:?}", embed.fields))
}

#[test]
fn a_failed_announce_does_not_render_as_a_successful_one() {
    // Everything published; OCX_ANNOUNCE_TOKEN has expired. The push exits
    // 0 and the job is green, so without this row Discord posts an embed
    // byte-identical to a run that announced — while the index never
    // learned about the release and the only other evidence is a
    // `::warning` and an artifact that expires in a day.
    let mut summary = make_all_green_summary();

    summary.announce = Some(AnnounceOutcome::Announced {
        package: "bazelbuild/bazelisk".to_string(),
        tags: vec!["1.21.0".to_string(), "1.21".to_string()],
        pull_request_url: Some("https://github.com/ocx-sh/index/pull/81".to_string()),
    });
    let announced = build_messages(&summary, None);

    summary.announce = Some(AnnounceOutcome::Failed {
        package: "bazelbuild/bazelisk".to_string(),
        error: "ocx package announce exited 70: bad credentials".to_string(),
    });
    let failed = build_messages(&summary, None);

    assert_ne!(
        announce_row(only_embed(&announced)).value,
        announce_row(only_embed(&failed)).value,
        "a failed announce must be distinguishable from a successful one",
    );

    let row = announce_row(only_embed(&failed));
    assert!(row.value.contains("announce failed"), "got: {}", row.value);
    assert!(row.value.contains("bad credentials"), "got: {}", row.value);
    assert!(!row.inline, "the Index row must not join the two-column layout");
}

#[test]
fn every_announce_state_reads_differently_and_absent_means_unconfigured() {
    let mut summary = make_all_green_summary();
    let package = "bazelbuild/bazelisk".to_string();

    let states = [
        AnnounceOutcome::Announced {
            package: package.clone(),
            tags: vec!["1.21.0".to_string()],
            pull_request_url: None,
        },
        AnnounceOutcome::AlreadyCurrent {
            package: package.clone(),
        },
        AnnounceOutcome::NothingToAnnounce {
            package: package.clone(),
        },
        AnnounceOutcome::SkippedNoCredential {
            package: package.clone(),
        },
        AnnounceOutcome::Failed {
            package: package.clone(),
            error: "boom".to_string(),
        },
        // A run killed mid-announce keeps the pre-announce placeholder.
        // It must not read as any of the four settled states, and above
        // all not as the absent key below — which means "never opted in".
        AnnounceOutcome::Interrupted { package },
    ];

    let mut rendered: Vec<String> = Vec::new();
    for state in states {
        summary.announce = Some(state);
        let messages = build_messages(&summary, None);
        rendered.push(announce_row(only_embed(&messages)).value.clone());
    }
    rendered.sort();
    let distinct = rendered.len();
    rendered.dedup();
    assert_eq!(rendered.len(), distinct, "every announce state must read differently");

    // No `announce:` block at all → no row, and the two-column layout is
    // untouched for every mirror that never opted in.
    summary.announce = None;
    let unconfigured = build_messages(&summary, None);
    let names: Vec<&str> = only_embed(&unconfigured)
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(names, vec!["Platform", "Status"]);
}

#[test]
fn the_announce_row_is_attached_once_per_run() {
    // One announce per run, so one row — on the first emitted embed, not
    // repeated down a multi-version backfill.
    let mut summary = make_all_green_summary();
    let mut second = version(VersionStatus::Published, "3.8.0");
    second.platforms_pushed = vec!["linux/amd64".to_string()];
    summary.versions.push(second);
    summary.announce = Some(AnnounceOutcome::SkippedNoCredential {
        package: "bazelbuild/bazelisk".to_string(),
    });

    let messages = build_messages(&summary, None);
    assert_eq!(messages.len(), 2, "one message per version");
    assert!(
        announce_row(&messages[0].embeds[0]).value.contains("not announced"),
        "the first embed carries the row",
    );
    assert!(
        !messages[1].embeds[0].fields.iter().any(|f| f.name == "Index"),
        "later embeds must not repeat it: {:?}",
        messages[1].embeds[0].fields,
    );
}
