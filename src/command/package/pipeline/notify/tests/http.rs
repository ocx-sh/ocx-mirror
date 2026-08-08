// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── HTTP-interaction tests (local TCP server) ──────────────────────────

fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Spawn a minimal HTTP server that accepts one request and responds with `status_code`.
async fn one_shot_server(status_code: u16) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/webhook");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let response = format!("HTTP/1.1 {status_code} \r\nContent-Length: 0\r\n\r\n");
        let _ = stream.write_all(response.as_bytes()).await;
    });

    url
}

/// Spawn a minimal HTTP server that accepts one TCP connection per status code in
/// `statuses`, replying with `Connection: close` so reqwest opens a fresh connection
/// per request. Increments `served` on each accepted connection. Returns the server URL.
async fn sequence_status_server(statuses: Vec<u16>, served: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/webhook");

    tokio::spawn(async move {
        for status_code in statuses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await;
            let response = format!("HTTP/1.1 {status_code} \r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
            let _ = stream.write_all(response.as_bytes()).await;
            served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    url
}

/// Drive `Notify::execute` against a stub TCP server bound to `OCX_MIRROR_DISCORD_HOOK`.
async fn post_to_stub(summary: &RunSummary, status_code: u16) -> Result<(), MirrorError> {
    ensure_crypto_provider();
    let server_url = one_shot_server(status_code).await;
    let _guard = WebhookEnvGuard::set(&server_url);

    let f = write_run_summary(summary);
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    let cmd = Notify {
        run_summary: f.path().to_path_buf(),
    };
    let result = cmd.execute(&printer).await;
    let _ = f;
    result
}

#[tokio::test]
async fn notify_posts_green_embed_for_all_new_green() {
    let result = post_to_stub(&make_all_green_summary(), 204).await;
    assert!(matches!(result, Ok(())), "2xx response must yield Ok(()): {result:?}");
}

#[tokio::test]
async fn notify_posts_yellow_embed_for_partial() {
    let result = post_to_stub(&make_partial_summary(), 200).await;
    assert!(matches!(result, Ok(())), "2xx response must yield Ok(()): {result:?}");
}

#[tokio::test]
async fn notify_posts_red_embed_for_all_failed() {
    let result = post_to_stub(&make_all_failed_summary(), 200).await;
    assert!(matches!(result, Ok(())), "2xx response must yield Ok(()): {result:?}");
}

#[tokio::test]
async fn notify_discord_5xx_returns_webhook_unavailable() {
    let result = post_to_stub(&make_all_green_summary(), 503).await;
    assert!(
        matches!(result, Err(MirrorError::WebhookUnavailable(_))),
        "5xx must return WebhookUnavailable: {result:?}"
    );
}

#[tokio::test]
async fn notify_discord_401_returns_webhook_permission_denied() {
    let result = post_to_stub(&make_all_green_summary(), 401).await;
    assert!(
        matches!(result, Err(MirrorError::WebhookPermissionDenied(_))),
        "401 must return WebhookPermissionDenied: {result:?}"
    );
}

#[tokio::test]
async fn notify_discord_403_returns_webhook_permission_denied() {
    let result = post_to_stub(&make_all_green_summary(), 403).await;
    assert!(
        matches!(result, Err(MirrorError::WebhookPermissionDenied(_))),
        "403 must return WebhookPermissionDenied: {result:?}"
    );
}

// B1: pacing loop — 2-version green summary → 2 POSTs with INTER_MESSAGE_DELAY between them.
#[tokio::test]
async fn notify_execute_posts_once_per_version_with_pacing() {
    ensure_crypto_provider();
    let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_url = sequence_status_server(vec![204, 204], served.clone()).await;
    let _guard = WebhookEnvGuard::set(&server_url);

    // Build a 2-version green summary.
    let versions = vec![
        {
            let mut version_entry = version(VersionStatus::Published, "1.0.0");
            version_entry.platforms_pushed = vec!["linux/amd64".to_string()];
            version_entry
        },
        {
            let mut version_entry = version(VersionStatus::Published, "1.0.1");
            version_entry.platforms_pushed = vec!["linux/amd64".to_string()];
            version_entry
        },
    ];
    let summary = run_summary(versions, true, false);

    let start = std::time::Instant::now();
    let f = write_run_summary(&summary);
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    let cmd = Notify {
        run_summary: f.path().to_path_buf(),
    };
    let result = cmd.execute(&printer).await;
    let elapsed = start.elapsed();
    let _ = f;

    assert!(matches!(result, Ok(())), "2-version green run must succeed: {result:?}");
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "execute must POST exactly once per version (2 total)"
    );
    assert!(
        elapsed >= INTER_MESSAGE_DELAY,
        "pacing delay must be applied between messages; elapsed: {elapsed:?}, expected >= {INTER_MESSAGE_DELAY:?}"
    );
    // Exactly one delay must elapse: a regression that also delayed before the
    // first message would push elapsed past two delays. Upper bound catches it
    // while leaving a full INTER_MESSAGE_DELAY of slack for setup + I/O.
    assert!(
        elapsed < INTER_MESSAGE_DELAY * 2,
        "only one inter-message delay must elapse (none before the first message); elapsed: {elapsed:?}, bound: {:?}",
        INTER_MESSAGE_DELAY * 2
    );
}

// B1: single-version run — no delay before the first (and only) message.
#[tokio::test]
async fn notify_execute_single_message_skips_pre_delay() {
    ensure_crypto_provider();
    let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_url = sequence_status_server(vec![204], served.clone()).await;
    let _guard = WebhookEnvGuard::set(&server_url);

    let summary = make_all_green_summary();
    let start = std::time::Instant::now();
    let f = write_run_summary(&summary);
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    let cmd = Notify {
        run_summary: f.path().to_path_buf(),
    };
    let result = cmd.execute(&printer).await;
    let elapsed = start.elapsed();
    let _ = f;

    assert!(matches!(result, Ok(())), "single-version run must succeed: {result:?}");
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "execute must POST exactly once for a single-version summary"
    );
    assert!(
        elapsed < INTER_MESSAGE_DELAY,
        "no inter-message delay before the first (only) message; elapsed: {elapsed:?}, bound: {INTER_MESSAGE_DELAY:?}"
    );
}

// Regression: clip panicked at `s[..budget]` when the byte at `budget`
// landed inside a multi-byte emoji codepoint (🟢 = 4 bytes).
#[test]
fn clip_to_field_limit_handles_emoji_at_byte_boundary() {
    let cell = "[`🟢`](https://example.com/job/1)";
    let mut s = String::new();
    while s.len() <= DISCORD_FIELD_VALUE_LIMIT {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(cell);
    }
    let clipped = clip_to_field_limit(&s);
    assert!(clipped.len() <= DISCORD_FIELD_VALUE_LIMIT);
    assert!(clipped.is_char_boundary(clipped.len()));
}
