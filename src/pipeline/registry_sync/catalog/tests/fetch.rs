// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-018…C-020 — the three index-tree fetches: the `config.json` format gate,
//! the catalog, the per-package root, and the body cap every one of them rides.

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;

/// A client for `index`, built through the production constructor so every
/// test runs against the real redirect policy and timeouts.
async fn client_for(index: &TestIndex) -> reqwest::Client {
    build_source_index_client(&index.base_url(), &loopback_trusted())
        .await
        .expect("a trusted loopback host builds a client")
}

// ── config.json (C-018) ──────────────────────────────────────────────────────

#[tokio::test]
async fn config_returns_the_bytes_verbatim_beside_the_parsed_shape() {
    // The sibling key is the point: the mirror re-serves these bytes, and a
    // parse → `serialize_config` round trip would drop what this ocx does not
    // model.
    let served = r#"{"format_version":1,"name_segments":2,"future_key":["x"]}"#;
    let index = TestIndex::start(vec![Route::json("/config.json", served)]).await;
    let client = client_for(&index).await;

    let (bytes, config) = fetch_source_config(&client, &index.base_url())
        .await
        .expect("a v1 config is served")
        .expect("the document is present");

    assert_eq!(bytes, served.as_bytes(), "the raw bytes must ride through untouched");
    assert_eq!(config.format_version, 1);
}

#[tokio::test]
async fn an_absent_config_is_not_fatal() {
    let index = TestIndex::start(vec![Route::status("/config.json", "404 Not Found")]).await;
    let client = client_for(&index).await;

    let absent = fetch_source_config(&client, &index.base_url())
        .await
        .expect("a 404 config is a synthesis signal, not a failure");

    assert!(absent.is_none(), "an absent config.json reads as None");
}

#[tokio::test]
async fn an_unknown_format_version_aborts_the_run() {
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":2}"#)]).await;
    let client = client_for(&index).await;

    let error = fetch_source_config(&client, &index.base_url())
        .await
        .expect_err("a format this ocx does not implement is a refusal, never a downgrade");

    assert!(
        matches!(error, MirrorError::IndexFormatUnsupported(2)),
        "got {error:?} — the version must ride out in the error"
    );
    // 65, not 69: a newer format is not transient, and retrying cannot fix it.
    assert_eq!(error.kind_exit_code(), ExitCode::DataError);
}

#[tokio::test]
async fn a_version_below_the_supported_one_is_refused_too() {
    // The gate is exact equality, not an upper bound: `0` is no more readable
    // than `2`, and "the number looks small, parse it anyway" is how foreign
    // data becomes control flow.
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":0}"#)]).await;
    let client = client_for(&index).await;

    let error = fetch_source_config(&client, &index.base_url())
        .await
        .expect_err("version 0 is not version 1");

    assert!(matches!(error, MirrorError::IndexFormatUnsupported(0)), "got {error:?}");
}

#[tokio::test]
async fn a_config_without_a_version_pin_is_refused() {
    // Absence of the *file* reads as v1; an empty *document* is malformed, and
    // defaulting it would silently admit an unversioned body as version 1.
    let index = TestIndex::start(vec![Route::json("/config.json", "{}")]).await;
    let client = client_for(&index).await;

    let error = fetch_source_config(&client, &index.base_url())
        .await
        .expect_err("a config with no format_version is not a config");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn an_unparseable_config_is_a_source_error() {
    let index = TestIndex::start(vec![Route::json("/config.json", "not json")]).await;
    let client = client_for(&index).await;

    let error = fetch_source_config(&client, &index.base_url())
        .await
        .expect_err("a non-JSON body is not a config");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    assert_eq!(error.kind_exit_code(), ExitCode::Unavailable);
}

// ── c/index.json (C-019) ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_catalog_returns_its_bytes_and_its_packages() {
    let served = r#"{"format_version":1,"packages":{"kitware/cmake":"sha256:abc","ns/pkg":"sha256:def"}}"#;
    let index = TestIndex::start(vec![Route::json("/c/index.json", served)]).await;
    let client = client_for(&index).await;

    let (bytes, catalog) = fetch_source_catalog(&client, &index.base_url())
        .await
        .expect("a v1 catalog is served");

    // The bytes are the short-circuit's cache key, so they must be the served
    // ones and not a re-serialization.
    assert_eq!(bytes, served.as_bytes());
    assert_eq!(catalog.packages.len(), 2);
    assert_eq!(
        catalog.packages.get("kitware/cmake").map(String::as_str),
        Some("sha256:abc")
    );
    assert_eq!(index.paths(), vec!["/c/index.json".to_string()]);
}

#[tokio::test]
async fn an_absent_catalog_is_fatal() {
    // Unlike `config.json`: there is no other way to enumerate the source.
    let index = TestIndex::start(vec![Route::status("/c/index.json", "404 Not Found")]).await;
    let client = client_for(&index).await;

    let error = fetch_source_catalog(&client, &index.base_url())
        .await
        .expect_err("a source with no catalog cannot be enumerated");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    assert_eq!(error.kind_exit_code(), ExitCode::Unavailable);
}

#[tokio::test]
async fn a_catalog_envelope_at_an_unknown_version_is_refused() {
    // The envelope carries the same pin `config.json` does. Gating only there
    // would let a source serve no `config.json` — which synthesizes v1 — and a
    // v2 catalog, with nothing checking the grammar every other read fans out
    // from.
    let index = TestIndex::start(vec![Route::json(
        "/c/index.json",
        r#"{"format_version":2,"packages":{"ns/pkg":"sha256:abc"}}"#,
    )])
    .await;
    let client = client_for(&index).await;

    let error = fetch_source_catalog(&client, &index.base_url())
        .await
        .expect_err("a v2 catalog is not readable by a v1 reader");

    assert!(matches!(error, MirrorError::IndexFormatUnsupported(2)), "got {error:?}");
    assert_eq!(error.kind_exit_code(), ExitCode::DataError);
}

#[tokio::test]
async fn an_empty_catalog_is_a_valid_catalog() {
    let index = TestIndex::start(vec![Route::json("/c/index.json", r#"{"format_version":1}"#)]).await;
    let client = client_for(&index).await;

    let (_, catalog) = fetch_source_catalog(&client, &index.base_url())
        .await
        .expect("a freshly deployed index publishes nothing yet");

    assert!(catalog.packages.is_empty());
}

// ── p/<ns>/<pkg>.json (C-020) ────────────────────────────────────────────────

#[tokio::test]
async fn a_root_returns_its_bytes_and_its_tags() {
    let served = format!(
        r#"{{"repository":"oci://ghcr.io/kitware/cmake","tags":{{"3.28.1":{{"content":"{FIXTURE_DIGEST}"}}}},"unmodelled":1}}"#
    );
    let index = TestIndex::start(vec![Route::json("/p/kitware/cmake.json", &served)]).await;
    let client = client_for(&index).await;

    let (bytes, root) = fetch_source_root(&client, &index.base_url(), "kitware/cmake")
        .await
        .expect("the root is served");

    assert_eq!(bytes, served.as_bytes(), "the rewrite operates on these bytes");
    assert_eq!(root.repository, "oci://ghcr.io/kitware/cmake");
    assert_eq!(root.tags.len(), 1);
    assert!(root.tags.contains_key("3.28.1"));
    assert_eq!(index.paths(), vec!["/p/kitware/cmake.json".to_string()]);
}

#[tokio::test]
async fn an_absent_root_is_an_error() {
    let index = TestIndex::start(Vec::new()).await;
    let client = client_for(&index).await;

    let error = fetch_source_root(&client, &index.base_url(), "kitware/cmake")
        .await
        .expect_err("a catalog naming a package whose root is absent is a source fault");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn an_unparseable_root_is_an_error() {
    let index = TestIndex::start(vec![Route::json("/p/ns/pkg.json", r#"{"tags":{}}"#)]).await;
    let client = client_for(&index).await;

    let error = fetch_source_root(&client, &index.base_url(), "ns/pkg")
        .await
        .expect_err("a root with no repository pointer is not a root");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn a_catalog_key_that_cannot_address_a_document_is_refused_before_the_fetch() {
    // The key is foreign data and this is where it becomes a URL path. Every
    // one of these would otherwise walk or truncate the fetch out of the
    // source's own `p/` subtree.
    let index = TestIndex::start(vec![Route::json(
        "/p/ns/pkg.json",
        r#"{"repository":"oci://ghcr.io/ns/pkg"}"#,
    )])
    .await;
    let client = client_for(&index).await;

    for hostile in [
        "../../etc/passwd",
        "ns/../../evil",
        "ns/pkg?x=1",
        "ns/pkg#frag",
        "%2e%2e/evil",
        "ns//pkg",
        "",
        "ns/pkg\\evil",
    ] {
        let error = fetch_source_root(&client, &index.base_url(), hostile)
            .await
            .expect_err("a key that cannot address a root document must be refused");
        assert!(matches!(error, MirrorError::SourceError(_)), "{hostile}: got {error:?}");
    }

    assert!(
        index.paths().is_empty(),
        "not one of the refusals may have issued a request"
    );

    // The green half: an ordinary two-segment key still addresses its document.
    fetch_source_root(&client, &index.base_url(), "ns/pkg")
        .await
        .expect("a legal catalog key still resolves");
    assert_eq!(index.paths(), vec!["/p/ns/pkg.json".to_string()]);
}

// ── the body cap, shared by all three (C-020) ────────────────────────────────

#[tokio::test]
async fn a_declared_oversize_body_is_refused_before_a_byte_is_read() {
    let index = TestIndex::start(vec![Route::declared_length("/big.json", "{}", 999_999)]).await;
    let client = client_for(&index).await;

    let error = fetch_index_document_capped(&client, &index.base_url(), "big.json", 8)
        .await
        .expect_err("a declared body over the cap is refused up front");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn a_streamed_oversize_body_is_refused_mid_stream() {
    // No `Content-Length` at all: the declared-size check cannot see this one,
    // so the running total is what has to catch it.
    let index = TestIndex::start(vec![Route::chunked("/big.json", &"x".repeat(64))]).await;
    let client = client_for(&index).await;

    let error = fetch_index_document_capped(&client, &index.base_url(), "big.json", 8)
        .await
        .expect_err("a chunked body over the cap is refused while streaming");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");

    // The green half on the same route: under a cap that fits, the same body
    // reads back whole — so the refusal above is the cap firing, not the
    // chunked encoding failing to parse.
    let body = fetch_index_document_capped(&client, &index.base_url(), "big.json", 64)
        .await
        .expect("a body inside the cap is served")
        .expect("the document is present");
    assert_eq!(body.len(), 64);
}

#[tokio::test]
async fn only_a_404_reads_as_absence() {
    // `Ok(None)` is a control-flow decision (`config.json` synthesis), so it
    // must not be inferred from a status the server did not send — including a
    // `304` answering this unconditional GET.
    let index = TestIndex::start(vec![
        Route::status("/not-modified.json", "304 Not Modified"),
        Route::status("/server-error.json", "500 Internal Server Error"),
        Route::status("/forbidden.json", "403 Forbidden"),
    ])
    .await;
    let client = client_for(&index).await;

    for relative in ["not-modified.json", "server-error.json", "forbidden.json"] {
        let error = fetch_index_document(&client, &index.base_url(), relative)
            .await
            .expect_err("only a confirmed 404 is absence");
        assert!(
            matches!(error, MirrorError::SourceError(_)),
            "{relative}: got {error:?}"
        );
    }

    let absent = fetch_index_document(&client, &index.base_url(), "missing.json")
        .await
        .expect("a 404 is a clean miss");
    assert!(absent.is_none());
}

// ── what a message may say about foreign input ───────────────────────────────

#[tokio::test]
async fn a_fetch_error_names_the_origin_and_document_never_the_base_url_path() {
    // A *capability URL* — the secret in the path — is a functional index base
    // that neither the userinfo rule (C-005) nor the https rule (C-006)
    // refuses, and this message is shipped in CI logs.
    let index = TestIndex::start(vec![Route::status(
        "/s3cr3t-capability/config.json",
        "500 Internal Server Error",
    )])
    .await;
    let base = format!("{}/s3cr3t-capability", index.base_url());
    let client = build_source_index_client(&base, &loopback_trusted())
        .await
        .expect("a trusted loopback host builds a client");

    let rendered = fetch_source_config(&client, &base)
        .await
        .expect_err("a 500 is not a document")
        .to_string();

    assert!(
        !rendered.contains("s3cr3t-capability"),
        "the base URL's path is a credential surface and must not be echoed: {rendered}"
    );
    assert!(
        rendered.contains(&index.base_url()) && rendered.contains("config.json"),
        "the message must still say which source and which document: {rendered}"
    );
}

#[tokio::test]
async fn a_refused_catalog_key_is_escaped_into_the_message() {
    // CWE-117. The key has passed no charset guard when it reaches this
    // message — that is the guard refusing it — so a newline in it would forge
    // a whole log line in the CI output an operator reads.
    let index = TestIndex::start(Vec::new()).await;
    let client = client_for(&index).await;

    let forged = fetch_source_root(
        &client,
        &index.base_url(),
        "ns/pkg\n[2026-08-14 INFO] copied 121/121 packages ok",
    )
    .await
    .expect_err("a newline is not in the OCI repository grammar")
    .to_string();
    assert!(
        !forged.contains('\n') && forged.contains("\\n"),
        "the key must be escaped, not echoed: {forged:?}"
    );

    let spoofed = fetch_source_root(&client, &index.base_url(), "ns/pkg\u{202e}")
        .await
        .expect_err("a direction override is not in the grammar either")
        .to_string();
    assert!(
        !spoofed.contains('\u{202e}') && spoofed.contains("\\u{202e}"),
        "the direction override reaches an operator's terminal: {spoofed:?}"
    );

    assert!(index.paths().is_empty(), "neither refusal may have issued a request");
}
