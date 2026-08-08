// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline notify` — read `run-summary.json` and POST Discord
//! webhook notifications per the D10 taxonomy.
//!
//! One Discord message per published version (one embed per message). Any
//! message carrying a partial/failed version is prefixed with an in-message
//! `<@id>` mention when `OCX_MIRROR_DISCORD_USER_ID` is set.

use std::path::PathBuf;
use std::time::Duration;

use ocx_lib::cli::DataInterface;

use crate::discord::{
    self, AllowedMentions, DiscordEmbed, DiscordEmbedAuthor, DiscordEmbedField, DiscordEmbedThumbnail,
    DiscordWebhookPayload,
};
use crate::error::MirrorError;
use crate::run_summary::{AnnounceOutcome, RunSummary, VersionStatus, VersionSummary};

/// `ocx-mirror package pipeline notify` subcommand.
///
/// Reads `run-summary.json` and posts to the Discord webhook URL sourced from
/// `$OCX_MIRROR_DISCORD_HOOK`. Silent (exit 0, no POST) when all versions are
/// `skipped_existing` and no test failures occurred.
#[derive(clap::Parser)]
pub struct Notify {
    /// Path to the `run-summary.json` produced by `pipeline push`.
    #[arg(long, required = true)]
    pub run_summary: PathBuf,
}

/// Conventional env var carrying the Discord webhook URL at runtime.
///
/// Hardcoded by design — spec's `notify.discord.webhook_secret` controls which
/// GitHub Actions secret maps onto this fixed name in the rendered workflow.
/// Keeping the local env var name fixed removes a layer of indirection (no
/// per-mirror flag, no env-name plumbing through the workflow template).
pub(crate) const WEBHOOK_ENV_VAR: &str = "OCX_MIRROR_DISCORD_HOOK";

/// Conventional env var carrying the Discord user ID (snowflake) to mention on
/// failures. Non-secret — the renderer inlines `notify.discord.user_id` into
/// the notify job env under this fixed name. Unset / empty → no mention.
pub(crate) const USER_ID_ENV_VAR: &str = "OCX_MIRROR_DISCORD_USER_ID";

impl Notify {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        // Read and parse run-summary.json.
        let raw = tokio::fs::read_to_string(&self.run_summary)
            .await
            .map_err(|e| MirrorError::RunSummaryError(format!("failed to read {}: {e}", self.run_summary.display())))?;
        let summary: RunSummary = serde_json::from_str(&raw)
            .map_err(|e| MirrorError::RunSummaryError(format!("malformed run-summary.json: {e}")))?;

        if summary.schema_version != 1 {
            return Err(MirrorError::RunSummaryError(format!(
                "unsupported run-summary.json schema_version {}; expected 1",
                summary.schema_version
            )));
        }

        // D10 rule: all skipped_existing (no new green, no red) → silent exit 0.
        if !summary.any_new_green && !summary.any_red {
            tracing::debug!("all versions skipped_existing; no notification to send");
            return Ok(());
        }

        // Optional mention target — non-secret, inlined into the workflow env.
        let user_id = std::env::var(USER_ID_ENV_VAR)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        let messages = build_messages(&summary, user_id.as_deref());
        if messages.is_empty() {
            tracing::debug!("no notifiable versions in run summary; nothing to send");
            return Ok(());
        }

        // Resolve webhook URL from the fixed environment variable.
        // URL is never logged — only the env var name may appear in messages.
        let webhook_url = std::env::var(WEBHOOK_ENV_VAR).map_err(|_| {
            MirrorError::SpecUsageError(format!(
                "environment variable '{WEBHOOK_ENV_VAR}' is not set; export it to the Discord webhook URL before running notify"
            ))
        })?;

        for (index, payload) in messages.iter().enumerate() {
            // Proactive rate-limit pacing: small inter-message delay between
            // consecutive POSTs. Skipped before the first message so single-
            // message runs incur no delay.
            if index > 0 {
                tokio::time::sleep(INTER_MESSAGE_DELAY).await;
            }
            discord::post(&webhook_url, payload).await?;
        }
        Ok(())
    }
}

/// Maximum length of a single Discord embed field value.
const DISCORD_FIELD_VALUE_LIMIT: usize = 1024;

/// Proactive inter-message delay between consecutive webhook POSTs.
///
/// One message per version multiplies POST count on backfills. A 750 ms gap
/// between messages stays well within Discord's per-webhook rate limit while
/// keeping CI notification latency acceptable for typical runs (1–5 versions).
/// Skipped before the first message so single-message runs incur no delay.
const INTER_MESSAGE_DELAY: Duration = Duration::from_millis(750);

/// Build the Discord messages for a run.
///
/// Emits one `DiscordWebhookPayload` per version that has rows (pushed /
/// failed / excluded) — exactly one embed per message. A message that carries
/// a partial/failed version is prefixed with `<@id>` (scoped via
/// `allowed_mentions`) when `user_id` is set. Returns an empty vec when no
/// version is notifiable (caller treats that as silent).
///
/// The author strip and thumbnail are attached only to the **first emitted**
/// embed so a multi-version run does not render a column of repeated logos.
fn build_messages(summary: &RunSummary, user_id: Option<&str>) -> Vec<DiscordWebhookPayload> {
    let mut messages: Vec<DiscordWebhookPayload> = Vec::new();
    for version in &summary.versions {
        // Decorate the first *emitted* embed with the author strip + thumbnail.
        // Keyed on `messages.is_empty()`, not the loop index: versions are stored
        // oldest-first, so versions[0] is often an already-published
        // SkippedExisting version that yields no embed — the decoration must
        // land on the first version that actually produces one.
        let is_first = messages.is_empty();
        if let Some(embed) = build_version_embed(summary, version, is_first) {
            let is_red = matches!(version.status, VersionStatus::Partial | VersionStatus::Failed);
            // Ping only when this version is failed/partial AND a user id is
            // configured. `parse: []` + explicit `users` scopes the ping to
            // that one user (no @everyone / role escalation).
            let (content, allowed_mentions) = match user_id.filter(|_| is_red) {
                Some(id) => (
                    Some(format!("<@{id}>")),
                    Some(AllowedMentions {
                        parse: vec![],
                        users: Some(vec![id.to_string()]),
                    }),
                ),
                None => (None, None),
            };
            messages.push(DiscordWebhookPayload {
                embeds: vec![embed],
                content,
                allowed_mentions,
            });
        }
    }
    messages
}

/// Build the embed for a single version, or `None` when it has no rows to show
/// (a skipped-existing version with no excluded platforms).
///
/// The title carries `{identifier}: {version} {state}`; the body is two inline
/// columns, Platform | Status. Status holds the 🟢/🔴/🚫 chip (linked to the
/// responsible GHA job) or a 🔒 row for a deliberately-excluded platform.
fn build_version_embed(summary: &RunSummary, version: &VersionSummary, decorate: bool) -> Option<DiscordEmbed> {
    let mut platforms: Vec<String> = Vec::new();
    let mut statuses: Vec<String> = Vec::new();

    for platform in &version.platforms_pushed {
        platforms.push(format!("`{platform}`"));
        statuses.push(outcome_cell(STATUS_SUCCESS, summary.push_job_url.as_deref()));
    }
    for failure in &version.platforms_failed {
        platforms.push(format!("`{}`", failure.platform));
        statuses.push(outcome_cell(
            status_glyph_for_reason(&failure.reason),
            failure.job_url.as_deref(),
        ));
    }
    for excluded in &version.platforms_excluded {
        platforms.push(format!("`{}`", excluded.platform));
        statuses.push(excluded_cell(excluded.reason.as_deref()));
    }

    if platforms.is_empty() {
        return None;
    }

    let (color, state) = version_color_and_state(&version.status);
    // Title is `{identifier}: {version} {state}`. Empty `target` falls back to
    // `mirror` so notify keeps a readable title even for a legacy summary.
    let identifier = if summary.target.trim().is_empty() {
        summary.mirror.as_str()
    } else {
        summary.target.as_str()
    };

    Some(DiscordEmbed {
        title: format!("{identifier}: {} {state}", version.version),
        color,
        url: Some(summary.run_url.clone()),
        description: None,
        author: decorate.then(|| build_author(summary)).flatten(),
        thumbnail: decorate.then(|| build_thumbnail(summary.logo_url.as_deref())).flatten(),
        fields: [
            Some(DiscordEmbedField {
                name: "Platform".to_string(),
                value: clip_to_field_limit(&platforms.join("\n")),
                inline: true,
            }),
            Some(DiscordEmbedField {
                name: "Status".to_string(),
                value: clip_to_field_limit(&statuses.join("\n")),
                inline: true,
            }),
            // Once per run, on the first emitted embed.
            decorate.then(|| announce_field(summary.announce.as_ref())).flatten(),
        ]
        .into_iter()
        .flatten()
        .collect(),
    })
}

/// Render the run's index-announce outcome as an embed field.
///
/// The announce is what makes a published version findable. Without this row a
/// failed or skipped announce is indistinguishable from a successful one: the
/// push is green either way, the embed says "published" either way, and the
/// only other trace is a `::warning` in a step log plus a JSON field in an
/// artifact that expires in a day. That is exactly the false green this
/// pipeline exists to prevent, so the outcome goes where the maintainer
/// actually looks.
///
/// Rendered for every state, not just the bad ones: an absent row would be
/// ambiguous between "announced fine" and "this mirror predates the field".
fn announce_field(announce: Option<&AnnounceOutcome>) -> Option<DiscordEmbedField> {
    let value = match announce? {
        AnnounceOutcome::Announced {
            package,
            tags,
            pull_request_url,
        } => match pull_request_url {
            Some(url) => format!(
                "`{STATUS_SUCCESS}` `{package}` — {} tag(s) announced ([PR]({url}))",
                tags.len()
            ),
            None => format!("`{STATUS_SUCCESS}` `{package}` — {} tag(s) announced", tags.len()),
        },
        AnnounceOutcome::AlreadyCurrent { package } => {
            format!("`{STATUS_EXCLUDED}` `{package}` — index already current, nothing changed")
        }
        AnnounceOutcome::NothingToAnnounce { package } => {
            format!("`{STATUS_EXCLUDED}` `{package}` — nothing new to announce")
        }
        AnnounceOutcome::SkippedNoCredential { package } => format!(
            "`{STATUS_MISSING}` `{package}` — **not announced**: no `OCX_ANNOUNCE_TOKEN`, the index does not know about this run"
        ),
        AnnounceOutcome::Failed { package, error } => {
            format!("`{STATUS_FAIL}` `{package}` — **announce failed**: {error}")
        }
        AnnounceOutcome::Interrupted { package } => format!(
            "`{STATUS_FAIL}` `{package}` — **announce interrupted**: the run was killed mid-call, the index may not know about this push"
        ),
    };
    Some(DiscordEmbedField {
        name: "Index".to_string(),
        value: clip_to_field_limit(&value),
        inline: false,
    })
}

/// Per-version color + state label for the embed title.
fn version_color_and_state(status: &VersionStatus) -> (u32, &'static str) {
    match status {
        VersionStatus::Published => (discord::colors::GREEN, "published"),
        VersionStatus::Partial => (discord::colors::YELLOW, "partial"),
        VersionStatus::Failed => (discord::colors::RED, "failed"),
        // Reached only when a skipped version still carries a 🔒 excluded row;
        // nothing failed this run, so render it green/informational.
        VersionStatus::SkippedExisting => (discord::colors::GREEN, "up to date"),
        VersionStatus::SkippedExecutor => (discord::colors::RED, "no executor"),
    }
}

/// Status icon for a row's terminal state. Code-styled (wrapped in
/// backticks at render time) so the chip matches the Platform column's rhythm.
const STATUS_SUCCESS: &str = "🟢";
const STATUS_FAIL: &str = "🔴";
const STATUS_MISSING: &str = "🚫";
/// A deliberately-excluded (`broken`) platform — not a failure, the gap is
/// declared in the spec via `platforms.<p>.exclude`.
const STATUS_EXCLUDED: &str = "🔒";

/// Pick the right Status icon for a `PlatformFailure.reason`.
///
/// `missing_bundle` / `missing_junit` express "expected artifact never
/// arrived" — a different shade of failure from a test that ran and failed.
/// The `🚫` glyph distinguishes them from genuine test/push failures.
fn status_glyph_for_reason(reason: &str) -> &'static str {
    match reason {
        "missing_bundle" | "missing_junit" => STATUS_MISSING,
        _ => STATUS_FAIL,
    }
}

/// Render the Status cell: a backtick-wrapped icon, made clickable when a
/// job URL is available. Inside markdown link text Discord still parses
/// inline code formatting, so `[``X``](url)` renders as a clickable
/// code-styled chip. Absent URL collapses to the plain code chip.
fn outcome_cell(glyph: &str, url: Option<&str>) -> String {
    let chip = format!("`{glyph}`");
    match url.map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) => format!("[{chip}]({u})"),
        None => chip,
    }
}

/// Render a 🔒 excluded-platform row: the lock chip plus the reason when given.
/// Not linked — there is no job to point at, the pair was never built.
fn excluded_cell(reason: Option<&str>) -> String {
    let chip = format!("`{STATUS_EXCLUDED}`");
    match reason.map(str::trim).filter(|s| !s.is_empty()) {
        Some(reason) => format!("{chip} {reason}"),
        None => chip,
    }
}

/// Build the embed author strip — a clickable link to the upstream project.
///
/// Renders only when `source_url` is set on the summary. Discord embed
/// thumbnails are decorative and cannot be hyperlinked; the author strip is
/// the conventional place for "click to view source". When the source URL
/// points at github.com we attach the owner's avatar as the author icon so
/// the strip renders with a recognisable face beside the link text.
fn build_author(summary: &RunSummary) -> Option<DiscordEmbedAuthor> {
    let url = summary.source_url.as_deref()?.trim();
    if url.is_empty() {
        return None;
    }
    let (name, icon_url) = match github_owner_repo(url) {
        Some((owner, repo)) => (
            format!("{owner}/{repo}"),
            Some(format!("https://github.com/{owner}.png?size=64")),
        ),
        None => ("View source".to_string(), None),
    };
    Some(DiscordEmbedAuthor {
        name,
        url: Some(url.to_string()),
        icon_url,
    })
}

/// Extract `(owner, repo)` from a github.com URL like
/// `https://github.com/mvdan/sh`. Returns `None` for non-github URLs or
/// malformed paths.
fn github_owner_repo(url: &str) -> Option<(&str, &str)> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut parts = path.trim_end_matches('/').splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Build the embed thumbnail from the run summary's `logo_url` field.
///
/// `pipeline push` computes the URL from `GITHUB_REPOSITORY` + `GITHUB_SHA`
/// so the link is pinned to the commit that produced the run (and therefore
/// resolves even when the mirror repo's `logo.png` hasn't landed on `main`
/// yet). Returns `None` when the field is unset or blank — Discord renders
/// the embed without a thumbnail in that case.
fn build_thumbnail(logo_url: Option<&str>) -> Option<DiscordEmbedThumbnail> {
    let url = logo_url?.trim();
    if url.is_empty() {
        return None;
    }
    Some(DiscordEmbedThumbnail { url: url.to_string() })
}

/// Clip a field value to the 1024-char Discord limit at the nearest newline.
///
/// Discord rejects any embed field whose value exceeds 1024 chars with HTTP
/// 400, so clipping is load-bearing — the cap itself isn't optional. The clip
/// rounds down to a UTF-8 char boundary so multi-byte emoji (🟢/🔴/🚫/🔒) at the
/// budget index don't panic in `s[..]`, then trims back to the last newline so
/// the cut lands between rows rather than mid-cell.
fn clip_to_field_limit(s: &str) -> String {
    if s.len() <= DISCORD_FIELD_VALUE_LIMIT {
        return s.to_string();
    }
    let boundary = (0..=DISCORD_FIELD_VALUE_LIMIT)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    let mut clipped = s[..boundary].to_string();
    if let Some(pos) = clipped.rfind('\n') {
        clipped.truncate(pos);
    }
    clipped
}

#[cfg(test)]
#[path = "notify/tests.rs"]
mod tests;
