// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-016 — the index-half network trust: the base-URL pre-flight that runs
//! before the client exists, the **pin** that makes its answer the answer the
//! socket uses, and the redirect policy that keeps the validated host the host
//! actually dialled.

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;

#[tokio::test]
async fn a_loopback_base_absent_from_trusted_hosts_is_refused() {
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":1}"#)]).await;

    let error = build_source_index_client(&index.base_url(), &[])
        .await
        .expect_err("loopback must be refused when no trusted_hosts entry opens it");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    assert_eq!(error.kind_exit_code(), ExitCode::Unavailable);
}

#[tokio::test]
async fn a_loopback_base_listed_in_trusted_hosts_is_allowed() {
    // The other half of the same guard: the escape hatch a corporate index on
    // an RFC1918 address depends on. Without this pair, a green refusal test
    // would also pass on a guard that refuses everything.
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":1}"#)]).await;

    build_source_index_client(&index.base_url(), &loopback_trusted())
        .await
        .expect("a trusted host passes the floor");
}

#[tokio::test]
async fn the_cloud_metadata_endpoint_is_refused() {
    install_crypto_provider();
    let error = build_source_index_client("http://169.254.169.254/", &[])
        .await
        .expect_err("the link-local metadata endpoint must be refused");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn an_rfc1918_base_is_refused_and_a_trusted_hosts_entry_opens_it() {
    install_crypto_provider();
    let error = build_source_index_client("https://10.1.2.3/index", &[])
        .await
        .expect_err("an RFC1918 address must be refused by default");
    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");

    // A CIDR entry is the shape `announce`'s `trusted_hosts` already accepts.
    build_source_index_client("https://10.1.2.3/index", &["10.0.0.0/8".to_string()])
        .await
        .expect("a CIDR trusted_hosts entry opens the same address");
}

#[tokio::test]
async fn a_hostless_base_is_refused() {
    install_crypto_provider();
    let error = build_source_index_client("file:///srv/index", &[])
        .await
        .expect_err("a URL with no host cannot be pre-flighted and must not build a client");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn an_unparseable_base_is_refused() {
    install_crypto_provider();
    let error = build_source_index_client("not a url", &[])
        .await
        .expect_err("an unparseable base URL must not build a client");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn a_refused_base_is_never_dialled() {
    // The pre-flight runs *before* the first fetch, so a refusal must cost zero
    // connections. The positive control first: a hand-made connection proves
    // the recorder can count, so the zero below is an observation and not a
    // recorder that never fires.
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":1}"#)]).await;
    let control = tokio::net::TcpStream::connect(index.address())
        .await
        .expect("the recorder must observe a real connection");
    drop(control);
    index.settle().await;
    assert_eq!(index.connection_count(), 1, "the control connection must be recorded");

    build_source_index_client(&index.base_url(), &[])
        .await
        .expect_err("loopback is refused without a trusted_hosts entry");

    index.settle().await;
    assert_eq!(
        index.connection_count(),
        1,
        "the refusal must not have dialled the index"
    );
    assert!(index.paths().is_empty(), "no request may have been issued");
}

// ── The pin: the validated answer is the answer that gets dialled ───────────

#[tokio::test]
async fn the_pre_flight_hands_back_the_addresses_it_validated() {
    // The defect this pins was a *discard*: `resolve_and_validate` returned the
    // judged addresses and the caller dropped them, leaving reqwest to resolve
    // the host a second time at connect (CWE-918 via CWE-367).
    let index = TestIndex::start(Vec::new()).await;
    let base = format!("http://localhost:{}", index.address().port());

    let pin = validate_index_base_host(&base, &["localhost".to_string()])
        .await
        .expect("a trusted host passes the floor")
        .expect("a DNS name earns a pin");

    assert_eq!(pin.0, "localhost");
    assert!(
        pin.1.iter().any(|address| address.ip().is_loopback()),
        "the validated addresses must ride out of the pre-flight: {:?}",
        pin.1
    );
}

#[tokio::test]
async fn an_ip_literal_base_earns_no_pin() {
    // There is no name for reqwest to resolve, so there is nothing to pin —
    // and an override keyed on an IP literal would never be consulted anyway.
    let index = TestIndex::start(Vec::new()).await;

    let pin = validate_index_base_host(&index.base_url(), &loopback_trusted())
        .await
        .expect("a trusted loopback host passes the floor");

    assert!(pin.is_none(), "an IP literal must not produce a DNS override: {pin:?}");
}

#[tokio::test]
async fn the_client_dials_the_pinned_address_instead_of_resolving_the_host() {
    // `.invalid` never resolves (RFC 6761 §6.4), so a fetch that lands can only
    // have used the pin. That is the whole property: between validation and
    // connect, nothing may consult DNS again.
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":1}"#)]).await;
    let base = format!("http://index.invalid:{}", index.address().port());

    let pinned = index_client(Some(("index.invalid".to_string(), vec![index.address()]))).expect("client builds");
    let config = fetch_source_config(&pinned, &base)
        .await
        .expect("the pinned address is dialled")
        .expect("the document is served");
    assert_eq!(config.1.format_version, 1);
    assert_eq!(index.paths(), vec!["/config.json".to_string()]);

    // The RED half, in the same test: without the override the same client has
    // only DNS to go on, and DNS has no answer for `.invalid`.
    let unpinned = index_client(None).expect("client builds");
    let error = fetch_source_config(&unpinned, &base)
        .await
        .expect_err("an unpinned client must resolve the name, and it does not resolve");
    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    assert_eq!(
        index.paths(),
        vec!["/config.json".to_string()],
        "the unpinned client must not have reached the server"
    );
}

#[tokio::test]
async fn the_pin_does_not_override_the_port_the_url_names() {
    // reqwest documents that the URL's port always wins over the override's,
    // which is what lets the pre-flight's `SocketAddr`s drop in unchanged. If
    // that ever stopped holding, every fetch would go to the wrong port.
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":1}"#)]).await;
    let wrong_port = std::net::SocketAddr::new(index.address().ip(), index.address().port().wrapping_add(1));
    let base = format!("http://index.invalid:{}", index.address().port());

    let client = index_client(Some(("index.invalid".to_string(), vec![wrong_port]))).expect("client builds");

    fetch_source_config(&client, &base)
        .await
        .expect("the URL's own port is the one dialled")
        .expect("the document is served");
}

#[tokio::test]
async fn the_index_client_does_not_follow_a_redirect() {
    let index = TestIndex::start(vec![
        Route::redirect("/config.json", "/moved.json"),
        Route::json("/moved.json", r#"{"format_version":1}"#),
    ])
    .await;

    let client = build_source_index_client(&index.base_url(), &loopback_trusted())
        .await
        .expect("a trusted loopback host builds a client");
    let error = fetch_source_config(&client, &index.base_url())
        .await
        .expect_err("a 302 is not a document");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    assert_eq!(
        index.paths(),
        vec!["/config.json".to_string()],
        "the redirect target must never be requested"
    );

    // The red half, in the same test: a client built WITHOUT the policy does
    // follow the same 302 and reads the relocated document — so the assertion
    // above is pinning the policy, not a server that cannot redirect.
    let following = reqwest::Client::builder()
        .build()
        .expect("a default-policy client builds");
    let config = fetch_source_config(&following, &index.base_url())
        .await
        .expect("the default policy follows the redirect")
        .expect("the relocated document is served");
    assert_eq!(config.1.format_version, 1);
    assert_eq!(
        index.paths(),
        vec![
            "/config.json".to_string(),
            "/config.json".to_string(),
            "/moved.json".to_string()
        ],
        "the unguarded client is what reaches the redirect target"
    );
}

#[tokio::test]
async fn a_base_url_with_a_trailing_slash_addresses_the_same_document() {
    let index = TestIndex::start(vec![Route::json("/config.json", r#"{"format_version":1}"#)]).await;
    let client = build_source_index_client(&index.base_url(), &loopback_trusted())
        .await
        .expect("a trusted loopback host builds a client");

    fetch_source_config(&client, &format!("{}/", index.base_url()))
        .await
        .expect("a trailing slash must not double up in the path")
        .expect("the document is served");

    assert_eq!(index.paths(), vec!["/config.json".to_string()]);
}
