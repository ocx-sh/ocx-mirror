// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Discord webhook payload types and HTTP POST helper.
//!
//! Used by `ocx-mirror pipeline notify` to report pipeline outcomes.
//!
//! # Payload shape
//!
//! ```json
//! {
//!   "embeds": [{ "title": "...", "color": 3066993, "fields": [...], "url": "..." }]
//! }
//! ```
//!
//! No `username` field is sent: Discord uses the webhook's configured bot
//! name (e.g. "Captain Mirror") so server admins control the bot identity.
//!
//! # Color codes
//!
//! | Status | Color | Hex |
//! |---|---|---|
//! | Published | Green | `0x2ECC71` (3066993) |
//! | Partial | Yellow | `0xF1C40F` (15844367) |
//! | Failed | Red | `0xE74C3C` (15158332) |

use serde::Serialize;

use crate::error::MirrorError;

/// A single field in a Discord embed.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordEmbedField {
    /// Field label.
    pub name: String,
    /// Field value.
    pub value: String,
    /// When `true`, Discord renders this field inline with adjacent inline fields.
    pub inline: bool,
}

/// Thumbnail image attached to a Discord embed.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordEmbedThumbnail {
    /// Public HTTPS URL of the image (Discord fetches it server-side).
    pub url: String,
}

/// Author strip rendered at the top of a Discord embed (above the title).
///
/// Renders as a small icon plus a clickable name. We use it to surface the
/// upstream project homepage, since Discord's `thumbnail` field is decorative
/// only — there is no way to make the thumbnail image itself a hyperlink.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordEmbedAuthor {
    /// Display text (clickable when `url` is set).
    pub name: String,
    /// URL the author name links to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Small icon shown to the left of `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// A Discord embed object.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordEmbed {
    /// Embed title (appears as the clickable link text when `url` is set).
    pub title: String,
    /// Decimal RGB color (e.g. `3066993` for `#2ECC71` green).
    pub color: u32,
    /// Optional URL that makes `title` a clickable link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Optional free-form text between the title and the fields. Used to
    /// surface the canonical OCI identifier (e.g. `ocx.sh/shfmt`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional author strip rendered above the title — used to link the
    /// embed back to the upstream project page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<DiscordEmbedAuthor>,
    /// Optional thumbnail image rendered in the top-right of the embed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<DiscordEmbedThumbnail>,
    /// Structured fields shown as a grid below the description.
    pub fields: Vec<DiscordEmbedField>,
}

/// Controls which mentions in `content` actually ping users.
///
/// With `parse: []` and `users: Some([id])`, only the listed user IDs ping —
/// `@everyone`, `@here`, and role mentions are suppressed even if present in
/// `content`. See the Discord allowed-mentions reference.
#[derive(Debug, Clone, Serialize)]
pub struct AllowedMentions {
    /// Allowed mention *types* to auto-parse from `content` (e.g. `"users"`,
    /// `"roles"`, `"everyone"`). Empty = parse none; only the explicit `users`
    /// list below may ping.
    pub parse: Vec<String>,
    /// Explicit user IDs permitted to ping, regardless of `parse`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<String>>,
}

/// Full Discord webhook payload.
///
/// No `username` override: Discord uses the webhook's configured bot name.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordWebhookPayload {
    /// Embeds for this message. Discord permits up to 10 per message.
    pub embeds: Vec<DiscordEmbed>,
    /// Optional message text rendered above the embeds. Carries the `<@id>`
    /// mention on messages that include a partial/failed version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Optional mention allow-list. Present only when `content` carries a
    /// `<@id>` ping, scoping the ping to that user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mentions: Option<AllowedMentions>,
}

/// Number of retries allowed when Discord responds with HTTP 429.
///
/// 3 retries = 4 total send attempts (initial + 3). On the 4th consecutive
/// 429, the request is abandoned and `WebhookUnavailable` is returned.
const MAX_RETRY_ATTEMPTS: u32 = 3;

/// Fixed jitter added on top of `Retry-After` to avoid thundering-herd
/// re-tries when multiple webhook POSTs are inflight simultaneously.
const RETRY_JITTER: std::time::Duration = std::time::Duration::from_millis(150);

/// Hard ceiling for any sleep derived from `Retry-After` or the JSON body
/// `retry_after` field. Prevents a malicious or misbehaving server from
/// parking the notifier for an unbounded duration.
const MAX_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_secs(60);

/// Hard cap on the 429 response body read before parsing `retry_after`.
/// Discord's 429 bodies are tiny JSON objects; bounding the read prevents a
/// hostile or misbehaving server from exhausting memory (CWE-400).
const MAX_RETRY_BODY_BYTES: usize = 8 * 1024;

/// POST a [`DiscordWebhookPayload`] to `webhook_url`.
///
/// Retries up to [`MAX_RETRY_ATTEMPTS`] times on HTTP 429, sleeping the
/// duration indicated by the `retry_after` JSON body field, the
/// `Retry-After` header, or 1 second as a fallback. Each sleep is capped at
/// [`MAX_RETRY_SLEEP`] and padded with [`RETRY_JITTER`].
///
/// # Errors
///
/// - [`MirrorError::WebhookUnavailable`] on 5xx, network timeout, or 429
///   after all retries are exhausted.
/// - [`MirrorError::WebhookPermissionDenied`] on 401/403.
pub async fn post(webhook_url: &str, payload: &DiscordWebhookPayload) -> Result<(), MirrorError> {
    // Build the client once; reuse across all retry attempts to benefit from
    // connection pooling and to avoid re-initialising TLS on every send.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| MirrorError::WebhookUnavailable(format!("failed to build HTTP client: {e}")))?;

    // Do not log the URL — webhook secret must not appear in traces or logs.
    tracing::debug!("posting to webhook");

    // Track whether any 429 carried `X-RateLimit-Scope: shared` so we can
    // surface it in the exhaustion error message.
    let mut saw_shared_scope = false;

    let mut attempt: u32 = 0;
    loop {
        let response = client.post(webhook_url).json(payload).send().await.map_err(|e| {
            // `reqwest::Error`'s Display embeds the request URL, whose path carries
            // the webhook secret — strip it before the message can reach logs.
            if e.is_timeout() {
                MirrorError::WebhookUnavailable(format!("request timed out: {}", e.without_url()))
            } else {
                MirrorError::WebhookUnavailable(format!("network error: {}", e.without_url()))
            }
        })?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let code = status.as_u16();
        if code == 401 || code == 403 {
            return Err(MirrorError::WebhookPermissionDenied(format!(
                "HTTP {code}: check webhook secret rotation"
            )));
        }

        if code == 429 {
            // Capture headers BEFORE consuming the body (consuming body moves
            // the response by value into `read_retry_after`).
            let scope_header = response
                .headers()
                .get("X-RateLimit-Scope")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let retry_after_header = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            if scope_header.as_deref() == Some("shared") {
                saw_shared_scope = true;
            }

            if attempt >= MAX_RETRY_ATTEMPTS {
                let scope_suffix = if saw_shared_scope { " (scope: shared)" } else { "" };
                return Err(MirrorError::WebhookUnavailable(format!(
                    "HTTP 429: rate limit exceeded after {MAX_RETRY_ATTEMPTS} retries{scope_suffix}"
                )));
            }

            let retry_after_secs = read_retry_after(response, retry_after_header).await;
            let sleep_duration = capped_sleep(retry_after_secs);
            tracing::debug!(
                attempt = attempt + 1,
                sleep_ms = sleep_duration.as_millis(),
                "HTTP 429 rate-limited; sleeping before retry"
            );
            tokio::time::sleep(sleep_duration).await;
            attempt += 1;
            continue;
        }

        if status.is_server_error() {
            return Err(MirrorError::WebhookUnavailable(format!("HTTP {code}: server error")));
        }
        // Other 4xx
        return Err(MirrorError::WebhookUnavailable(format!(
            "HTTP {code}: unexpected client error"
        )));
    }
}

/// Resolve the retry delay in seconds from a 429 response.
///
/// Resolution order (first match wins):
/// 1. JSON body field `{"retry_after": <f64>}` — Discord's authoritative value.
///    Body read is capped at [`MAX_RETRY_BODY_BYTES`]; if the body exceeds the
///    cap or a chunk read errors, this step is skipped (CWE-400).
/// 2. `Retry-After` header parsed as integer seconds (captured before this call
///    because consuming the body moves the response by value).
/// 3. Default of `1.0` second.
///
/// The returned value is NOT capped here; callers apply [`MAX_RETRY_SLEEP`].
async fn read_retry_after(mut response: reqwest::Response, retry_after_header: Option<u64>) -> f64 {
    // Stream-read the body with a hard cap to avoid OOM from a hostile server.
    let mut body_buf: Vec<u8> = Vec::new();
    let mut over_limit = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body_buf.len() + chunk.len() > MAX_RETRY_BODY_BYTES {
                    over_limit = true;
                    break;
                }
                body_buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                // A mid-body read error (connection reset, TLS) is non-fatal: the
                // outer retry loop re-attempts. Trace it — never swallow silently —
                // and strip the URL, whose path carries the webhook secret.
                tracing::debug!(error = %error.without_url(), "failed to read 429 body; falling back to header");
                over_limit = true;
                break;
            }
        }
    }

    if !over_limit {
        #[derive(serde::Deserialize)]
        struct RateLimitBody {
            retry_after: f64,
        }
        if let Ok(body) = serde_json::from_slice::<RateLimitBody>(&body_buf) {
            return body.retry_after;
        }
    }
    // Fall back to the `Retry-After` header (integer seconds).
    if let Some(secs) = retry_after_header {
        return secs as f64;
    }
    // Default: sleep 1 second before retrying.
    1.0
}

/// Convert a raw `retry_after` (seconds) into a bounded sleep duration.
///
/// Clamps to `[0, MAX_RETRY_SLEEP]` *before* constructing the `Duration`:
/// `Duration::from_secs_f64` panics on a negative or non-finite argument, and an
/// absurd server value would otherwise park the notifier for far too long. Fixed
/// jitter is then added to spread out simultaneous retries.
fn capped_sleep(retry_after_secs: f64) -> std::time::Duration {
    let finite = if retry_after_secs.is_finite() {
        retry_after_secs
    } else {
        0.0
    };
    let capped = finite.clamp(0.0, MAX_RETRY_SLEEP.as_secs_f64());
    std::time::Duration::from_secs_f64(capped) + RETRY_JITTER
}

/// Discord color codes per D10 taxonomy.
pub mod colors {
    /// Green: `#2ECC71` (3066993) — published successfully.
    pub const GREEN: u32 = 0x2ECC71;
    /// Yellow: `#F1C40F` (15844367) — partial push (some platforms failed).
    pub const YELLOW: u32 = 0xF1C40F;
    /// Red: `#E74C3C` (15158332) — all platforms failed.
    pub const RED: u32 = 0xE74C3C;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── §3.9 S9: Discord payload type tests ────────────────────────────────

    #[test]
    fn discord_embed_field_serializes_correctly() {
        // §3.9: Embed field structure matches design spec §2.5 payload shape
        let field = DiscordEmbedField {
            name: "Platforms".to_string(),
            value: "linux/amd64, darwin/arm64".to_string(),
            inline: false,
        };
        let value: serde_json::Value = serde_json::to_value(&field).unwrap();
        assert_eq!(value["name"].as_str().unwrap(), "Platforms");
        assert_eq!(value["value"].as_str().unwrap(), "linux/amd64, darwin/arm64");
        assert!(!value["inline"].as_bool().unwrap());
    }

    #[test]
    fn discord_embed_serializes_with_all_required_fields() {
        // §3.9: Discord embed JSON shape matches design spec §2.5
        let embed = DiscordEmbed {
            title: "📦 cmake: published 3.29.0".to_string(),
            color: colors::GREEN,
            url: Some("https://github.com/ocx-sh/mirror-cmake/actions/runs/12345".to_string()),
            description: None,
            author: None,
            thumbnail: None,
            fields: vec![
                DiscordEmbedField {
                    name: "Platforms".to_string(),
                    value: "linux/amd64, linux/arm64".to_string(),
                    inline: false,
                },
                DiscordEmbedField {
                    name: "Cascade".to_string(),
                    value: "3.29.0, 3.29, 3, latest".to_string(),
                    inline: false,
                },
            ],
        };

        let value: serde_json::Value = serde_json::to_value(&embed).unwrap();
        assert_eq!(value["title"].as_str().unwrap(), "📦 cmake: published 3.29.0");
        assert_eq!(value["color"].as_u64().unwrap(), colors::GREEN as u64);
        assert!(value.get("url").is_some(), "url must be present");
        assert_eq!(value["fields"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn discord_embed_url_omitted_when_none() {
        // §3.9: url field is skip_serializing_if = None
        let embed = DiscordEmbed {
            title: "Test".to_string(),
            color: colors::RED,
            url: None,
            description: None,
            author: None,
            thumbnail: None,
            fields: vec![],
        };
        let value: serde_json::Value = serde_json::to_value(&embed).unwrap();
        assert!(value.get("url").is_none(), "url must be absent when None (not null)");
    }

    #[test]
    fn discord_embed_thumbnail_omitted_when_none() {
        // Thumbnail is optional and must be absent (not null) when unset.
        let embed = DiscordEmbed {
            title: "Test".to_string(),
            color: colors::GREEN,
            url: None,
            description: None,
            author: None,
            thumbnail: None,
            fields: vec![],
        };
        let value: serde_json::Value = serde_json::to_value(&embed).unwrap();
        assert!(
            value.get("thumbnail").is_none(),
            "thumbnail must be absent when None: {value}"
        );
    }

    #[test]
    fn discord_embed_thumbnail_serializes_with_url() {
        let embed = DiscordEmbed {
            title: "Test".to_string(),
            color: colors::GREEN,
            url: None,
            description: None,
            author: None,
            thumbnail: Some(DiscordEmbedThumbnail {
                url: "https://raw.githubusercontent.com/owner/repo/main/logo.svg".to_string(),
            }),
            fields: vec![],
        };
        let value: serde_json::Value = serde_json::to_value(&embed).unwrap();
        let thumb = value.get("thumbnail").expect("thumbnail must be present when Some");
        assert_eq!(
            thumb["url"].as_str().unwrap(),
            "https://raw.githubusercontent.com/owner/repo/main/logo.svg"
        );
    }

    #[test]
    fn discord_payload_serializes_without_username_field() {
        // Webhook bot name (e.g. "Captain Mirror") is owned by server admins;
        // payload never carries a `username` override.
        let payload = DiscordWebhookPayload {
            embeds: vec![],
            content: None,
            allowed_mentions: None,
        };
        let value: serde_json::Value = serde_json::to_value(&payload).unwrap();
        assert!(value.get("username").is_none(), "username must be absent: {value}");
    }

    #[test]
    fn payload_omits_content_and_allowed_mentions_when_none() {
        let payload = DiscordWebhookPayload {
            embeds: vec![],
            content: None,
            allowed_mentions: None,
        };
        let value: serde_json::Value = serde_json::to_value(&payload).unwrap();
        assert!(value.get("content").is_none(), "content omitted when None: {value}");
        assert!(
            value.get("allowed_mentions").is_none(),
            "allowed_mentions omitted when None: {value}"
        );
    }

    #[test]
    fn payload_serializes_content_and_scoped_allowed_mentions() {
        let payload = DiscordWebhookPayload {
            embeds: vec![],
            content: Some("<@123456789012345678>".to_string()),
            allowed_mentions: Some(AllowedMentions {
                parse: vec![],
                users: Some(vec!["123456789012345678".to_string()]),
            }),
        };
        let value: serde_json::Value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["content"].as_str(), Some("<@123456789012345678>"));
        // `parse: []` suppresses @everyone/role pings; only the explicit user pings.
        assert_eq!(value["allowed_mentions"]["parse"].as_array().unwrap().len(), 0);
        assert_eq!(
            value["allowed_mentions"]["users"][0].as_str(),
            Some("123456789012345678")
        );
    }

    #[test]
    fn allowed_mentions_omits_users_when_none() {
        let am = AllowedMentions {
            parse: vec![],
            users: None,
        };
        let value: serde_json::Value = serde_json::to_value(&am).unwrap();
        assert!(value.get("users").is_none(), "users omitted when None: {value}");
    }

    #[test]
    fn green_color_matches_design_spec() {
        // §3.9: Color codes must match design spec §2.5 exactly
        assert_eq!(colors::GREEN, 0x2ECC71, "GREEN must be 0x2ECC71 (3066993)");
        assert_eq!(colors::GREEN, 3_066_993);
    }

    #[test]
    fn yellow_color_matches_design_spec() {
        assert_eq!(colors::YELLOW, 0xF1C40F, "YELLOW must be 0xF1C40F (15844367)");
        assert_eq!(colors::YELLOW, 15_844_367);
    }

    #[test]
    fn red_color_matches_design_spec() {
        assert_eq!(colors::RED, 0xE74C3C, "RED must be 0xE74C3C (15158332)");
        assert_eq!(colors::RED, 15_158_332);
    }

    // ── §3.9 S9: discord::post() HTTP tests ────────────────────────────────

    /// Install the rustls crypto provider if not already installed.
    ///
    /// Tests run without `main()`, so the provider must be initialized explicitly.
    /// `install_default()` returns `Err` if already set — silently ignore.
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    /// Spawn a minimal one-shot HTTP server that accepts one request and responds
    /// with `status_code`. Returns the server URL.
    async fn one_shot_http_server(status_code: u16) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/webhook");

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await;
            let response = format!("HTTP/1.1 {status_code} \r\nContent-Length: 0\r\n\r\n");
            let _ = stream.write_all(response.as_bytes()).await;
        });

        url
    }

    #[tokio::test]
    async fn discord_post_2xx_returns_ok() {
        // §3.9: 2xx response from Discord → Ok(())
        ensure_crypto_provider();
        let url = one_shot_http_server(204).await;
        let payload = DiscordWebhookPayload {
            embeds: vec![],
            content: None,
            allowed_mentions: None,
        };
        let result = post(&url, &payload).await;
        assert!(matches!(result, Ok(())), "2xx must return Ok(()): {result:?}");
    }

    #[tokio::test]
    async fn discord_post_5xx_returns_webhook_unavailable() {
        // §3.9: 5xx → WebhookUnavailable (exit 69)
        ensure_crypto_provider();
        let url = one_shot_http_server(503).await;
        let payload = DiscordWebhookPayload {
            embeds: vec![],
            content: None,
            allowed_mentions: None,
        };
        let result = post(&url, &payload).await;
        assert!(
            matches!(result, Err(MirrorError::WebhookUnavailable(_))),
            "5xx must return WebhookUnavailable: {result:?}"
        );
    }

    #[tokio::test]
    async fn discord_post_401_returns_permission_denied() {
        // §3.9: 401 → WebhookPermissionDenied (exit 77)
        ensure_crypto_provider();
        let url = one_shot_http_server(401).await;
        let payload = DiscordWebhookPayload {
            embeds: vec![],
            content: None,
            allowed_mentions: None,
        };
        let result = post(&url, &payload).await;
        assert!(
            matches!(result, Err(MirrorError::WebhookPermissionDenied(_))),
            "401 must return WebhookPermissionDenied: {result:?}"
        );
    }

    #[tokio::test]
    async fn discord_post_403_returns_permission_denied() {
        // §3.9: 403 → WebhookPermissionDenied (exit 77)
        ensure_crypto_provider();
        let url = one_shot_http_server(403).await;
        let payload = DiscordWebhookPayload {
            embeds: vec![],
            content: None,
            allowed_mentions: None,
        };
        let result = post(&url, &payload).await;
        assert!(
            matches!(result, Err(MirrorError::WebhookPermissionDenied(_))),
            "403 must return WebhookPermissionDenied: {result:?}"
        );
    }

    // ── C2: rate-limit (HTTP 429) retry tests ──────────────────────────────
    //
    // `sequence_stub_server` returns a URL whose server replies to successive
    // requests with successive entries from `responses`. Each TCP connection
    // is treated as one request; `Connection: close` forces reqwest to open a
    // fresh connection per request so the accept loop advances the sequence
    // correctly.

    /// A pre-canned response for the sequence stub server.
    struct StubResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Option<String>,
    }

    /// Spawn a minimal HTTP server that handles one request per TCP connection,
    /// replying in sequence from `responses`. `Connection: close` is sent on
    /// every response so reqwest opens a fresh TCP connection for each retry,
    /// advancing the accept loop to the next item.
    async fn sequence_stub_server(responses: Vec<StubResponse>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/webhook");

        tokio::spawn(async move {
            for stub in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;

                let body_bytes = stub.body.as_deref().unwrap_or("").as_bytes().to_vec();
                let content_length = body_bytes.len();

                let mut response = format!(
                    "HTTP/1.1 {} \r\nConnection: close\r\nContent-Length: {content_length}\r\n",
                    stub.status
                );
                for (name, value) in &stub.headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");

                let _ = stream.write_all(response.as_bytes()).await;
                if !body_bytes.is_empty() {
                    let _ = stream.write_all(&body_bytes).await;
                }
            }
        });

        url
    }

    fn empty_payload() -> DiscordWebhookPayload {
        DiscordWebhookPayload {
            embeds: vec![],
            content: None,
            allowed_mentions: None,
        }
    }

    /// A 429 asking for an immediate retry (`retry_after: 0`) so the retry path
    /// is exercised in real time without a wall-clock wait beyond the jitter.
    fn rate_limited() -> StubResponse {
        StubResponse {
            status: 429,
            headers: vec![],
            body: Some(r#"{"retry_after":0}"#.to_string()),
        }
    }

    fn ok_200() -> StubResponse {
        StubResponse {
            status: 200,
            headers: vec![],
            body: None,
        }
    }

    // C2: one 429 then 200 → Ok(()).
    // Verifies that `discord::post` retries on 429 instead of returning an error.
    #[tokio::test]
    async fn discord_post_429_then_200_returns_ok() {
        ensure_crypto_provider();
        let url = sequence_stub_server(vec![rate_limited(), ok_200()]).await;
        let result = post(&url, &empty_payload()).await;
        assert!(matches!(result, Ok(())), "429 then 200 must return Ok(()): {result:?}");
    }

    // C2: three 429s then 200 → Ok(()) — boundary: 3 retries succeed.
    #[tokio::test]
    async fn discord_post_429_thrice_then_200_returns_ok() {
        ensure_crypto_provider();
        let url = sequence_stub_server(vec![rate_limited(), rate_limited(), rate_limited(), ok_200()]).await;
        let result = post(&url, &empty_payload()).await;
        assert!(
            matches!(result, Ok(())),
            "3 retries (4 total attempts) must still succeed: {result:?}"
        );
    }

    // C2: four 429s → WebhookUnavailable — retries exhausted after 3 retries.
    #[tokio::test]
    async fn discord_post_429_four_times_returns_webhook_unavailable() {
        ensure_crypto_provider();
        let url = sequence_stub_server(vec![rate_limited(), rate_limited(), rate_limited(), rate_limited()]).await;
        let result = post(&url, &empty_payload()).await;
        assert!(
            matches!(result, Err(MirrorError::WebhookUnavailable(_))),
            "4 consecutive 429s (3 retries exhausted) must return WebhookUnavailable: {result:?}"
        );
    }

    // C2: 429 with JSON body `{"retry_after": 0.05}` then 200 → Ok(()).
    // Verifies that the JSON body `retry_after` field is parsed and honored.
    #[tokio::test]
    async fn discord_post_429_honors_json_body_retry_after() {
        ensure_crypto_provider();
        let url = sequence_stub_server(vec![
            StubResponse {
                status: 429,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body: Some(r#"{"retry_after":0.05}"#.to_string()),
            },
            StubResponse {
                status: 200,
                headers: vec![],
                body: None,
            },
        ])
        .await;
        let result = post(&url, &empty_payload()).await;
        assert!(
            matches!(result, Ok(())),
            "429 with json retry_after body then 200 must return Ok(()): {result:?}"
        );
    }

    // C2: 429 with `Retry-After: 1` header (no body) then 200 → Ok(()).
    // Verifies fallback to `Retry-After` header when body is absent.
    #[tokio::test]
    async fn discord_post_429_falls_back_to_retry_after_header() {
        ensure_crypto_provider();
        let url = sequence_stub_server(vec![
            StubResponse {
                status: 429,
                headers: vec![("Retry-After".to_string(), "1".to_string())],
                body: None,
            },
            StubResponse {
                status: 200,
                headers: vec![],
                body: None,
            },
        ])
        .await;
        let result = post(&url, &empty_payload()).await;
        assert!(
            matches!(result, Ok(())),
            "429 with Retry-After header then 200 must return Ok(()): {result:?}"
        );
    }

    // C2: four 429s with `X-RateLimit-Scope: shared` → WebhookUnavailable
    // whose message contains "shared".
    #[tokio::test]
    async fn discord_post_429_shared_scope_in_error() {
        ensure_crypto_provider();
        let shared_response = || StubResponse {
            status: 429,
            headers: vec![("X-RateLimit-Scope".to_string(), "shared".to_string())],
            body: Some(r#"{"retry_after":0}"#.to_string()),
        };
        let url = sequence_stub_server(vec![
            shared_response(),
            shared_response(),
            shared_response(),
            shared_response(),
        ])
        .await;
        let result = post(&url, &empty_payload()).await;
        match result {
            Err(MirrorError::WebhookUnavailable(msg)) => {
                assert!(
                    msg.contains("shared"),
                    "error message must mention 'shared' rate-limit scope, got: {msg:?}"
                );
            }
            other => panic!("expected WebhookUnavailable with 'shared' in message, got: {other:?}"),
        }
    }

    // W2: 429 with `Retry-After: 0` header and no body → header honored, not the 1.0s default.
    // The 1.0s default would exceed 750ms; header `0` → only RETRY_JITTER (~150ms) elapses.
    #[tokio::test]
    async fn discord_post_429_retry_after_header_zero_honored() {
        ensure_crypto_provider();
        let url = sequence_stub_server(vec![
            StubResponse {
                status: 429,
                headers: vec![("Retry-After".to_string(), "0".to_string())],
                body: None,
            },
            ok_200(),
        ])
        .await;
        let start = std::time::Instant::now();
        let result = post(&url, &empty_payload()).await;
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Ok(())),
            "429 + Retry-After:0 then 200 must return Ok(()): {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(750),
            "header Retry-After:0 must resolve faster than 750ms (1.0s default would exceed this); elapsed: {elapsed:?}"
        );
    }

    // W3: 429 with JSON body `{"retry_after":0}` AND `Retry-After: 60` header → body wins.
    // Body `0` → only jitter; header `60` would cap to 60s and blow the 1s bound.
    #[tokio::test]
    async fn discord_post_429_body_retry_after_beats_header() {
        ensure_crypto_provider();
        let url = sequence_stub_server(vec![
            StubResponse {
                status: 429,
                headers: vec![("Retry-After".to_string(), "60".to_string())],
                body: Some(r#"{"retry_after":0}"#.to_string()),
            },
            ok_200(),
        ])
        .await;
        let start = std::time::Instant::now();
        let result = post(&url, &empty_payload()).await;
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Ok(())),
            "body retry_after:0 + header Retry-After:60 then 200 must return Ok(()): {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "JSON body retry_after:0 must win over header Retry-After:60; elapsed: {elapsed:?}"
        );
    }

    // C2: `capped_sleep` clamps adversarial `retry_after` values without panicking.
    // Pure function — no network, no real sleep — covers the negative/non-finite
    // panic guard and the `MAX_RETRY_SLEEP` ceiling on the hot path.
    #[test]
    fn capped_sleep_clamps_negative_and_oversized_values() {
        // Negative would panic `Duration::from_secs_f64`; clamped to 0 (+ jitter).
        assert_eq!(capped_sleep(-1.0), RETRY_JITTER);
        // Absurd value is capped at the 60s ceiling (+ jitter), never longer.
        assert_eq!(capped_sleep(9_999.0), MAX_RETRY_SLEEP + RETRY_JITTER);
        // A normal sub-ceiling value passes through unchanged (+ jitter).
        assert_eq!(
            capped_sleep(0.05),
            std::time::Duration::from_secs_f64(0.05) + RETRY_JITTER
        );
        // Non-finite (defensive — JSON cannot carry NaN) folds to 0 (+ jitter).
        assert_eq!(capped_sleep(f64::NAN), RETRY_JITTER);
        // +Inf is also non-finite; same guard applies.
        assert_eq!(capped_sleep(f64::INFINITY), RETRY_JITTER);
    }
}
