// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-017 — the registry-half guard. A root's `repository` pointer is foreign
//! data: an upstream index telling the mirror which host to dial. It is
//! validated **before any registry request**, and a refusal aborts the run.

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;

/// A root document carrying `repository`, parsed from the wire shape rather
/// than constructed field-by-field — `IndexRoot` is a `Deserialize`-only type
/// and the fixture doubles as a check that the shape still parses.
fn root_pointing_at(repository: &str) -> IndexRoot {
    serde_json::from_str(&format!(
        r#"{{"repository":"{repository}","tags":{{"1.0":{{"content":"{FIXTURE_DIGEST}"}}}}}}"#
    ))
    .expect("the fixture is a well-formed root document")
}

#[tokio::test]
async fn a_loopback_pointer_is_refused() {
    let error = validate_root_host(&root_pointing_at("oci://127.0.0.1/ns/pkg"), &[])
        .await
        .expect_err("a root may not steer the mirror at loopback");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    // Diverges from `announce`, which maps the same `SsrfError` to 78: this
    // pointer came off a foreign index over the network, so the source is what
    // is misbehaving.
    assert_eq!(error.kind_exit_code(), ExitCode::Unavailable);
}

#[tokio::test]
async fn a_hostname_resolving_to_loopback_is_refused() {
    // The string check alone would pass this one — the refusal has to come
    // from the resolved address, which is what `resolve_and_validate` does.
    let error = validate_root_host(&root_pointing_at("oci://localhost:5000/ns/pkg"), &[])
        .await
        .expect_err("localhost resolves to loopback and must be refused");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn an_rfc1918_pointer_is_refused() {
    for private in [
        "oci://10.0.0.1/ns/pkg",
        "oci://192.168.1.1/ns/pkg",
        "oci://172.16.0.1/ns/pkg",
    ] {
        let error = validate_root_host(&root_pointing_at(private), &[])
            .await
            .expect_err("an RFC1918 pointer must be refused");
        assert!(matches!(error, MirrorError::SourceError(_)), "{private}: got {error:?}");
    }
}

#[tokio::test]
async fn the_link_local_metadata_endpoint_is_refused() {
    let error = validate_root_host(&root_pointing_at("oci://169.254.169.254/ns/pkg"), &[])
        .await
        .expect_err("the cloud metadata endpoint is the canonical SSRF target");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
}

#[tokio::test]
async fn a_trusted_hosts_entry_opens_the_same_pointer() {
    // The green half of the loopback refusal above, on the same input: the
    // guard discriminates, it does not refuse everything.
    validate_root_host(&root_pointing_at("oci://127.0.0.1/ns/pkg"), &["127.0.0.1".to_string()])
        .await
        .expect("a listed host skips the floor");
}

/// A bracketed IPv6 authority is the one shape `split_host_port` hands back
/// unusable: it yields the host `"[::1]"`, which parses as neither an IP
/// literal nor a DNS name.
///
/// That fails **closed**, so the refusal half was never the bug — the bug is
/// that no `trusted_hosts` entry could ever open it again, because an operator
/// writes `::1`, not `[::1]`. Both halves are asserted, because a green
/// refusal test also passes on a guard that refuses everything.
#[tokio::test]
async fn a_bracketed_ipv6_pointer_is_judged_on_the_address_inside_the_brackets() {
    let loopback = root_pointing_at("oci://[::1]:5000/ns/pkg");

    let error = validate_root_host(&loopback, &[])
        .await
        .expect_err("the IPv6 loopback is as forbidden as 127.0.0.1");
    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");

    validate_root_host(&loopback, &["::1".to_string()])
        .await
        .expect("the entry an operator would actually write must open it");
}

#[tokio::test]
async fn a_public_pointer_passes() {
    validate_root_host(&root_pointing_at("oci://8.8.8.8/ns/pkg"), &[])
        .await
        .expect("a public address is not an SSRF target");
}

#[tokio::test]
async fn a_malformed_pointer_is_refused_without_resolving_anything() {
    for malformed in [
        "ghcr.io/ns/pkg",           // no scheme — the `oci://` prefix is a wire contract
        "oci://ghcr.io",            // no path
        "oci:///ns/pkg",            // no host
        "oci://ghcr.io/ns/pkg:1.0", // a smuggled tag
    ] {
        let error = validate_root_host(&root_pointing_at(malformed), &[])
            .await
            .expect_err("a malformed repository pointer must be refused");
        assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    }
}

#[tokio::test]
async fn the_refusal_lands_before_any_connection_is_opened() {
    // "Before any registry request" is an ordering claim, so it gets a
    // recorder. The control connection first: without it a zero could mean the
    // recorder never fires.
    let index = TestIndex::start(Vec::new()).await;
    let control = tokio::net::TcpStream::connect(index.address())
        .await
        .expect("the recorder must observe a real connection");
    drop(control);
    index.settle().await;
    assert_eq!(index.connection_count(), 1, "the control connection must be recorded");

    let pointer = format!("oci://127.0.0.1:{}/ns/pkg", index.address().port());
    validate_root_host(&root_pointing_at(&pointer), &[])
        .await
        .expect_err("loopback is refused");

    index.settle().await;
    assert_eq!(
        index.connection_count(),
        1,
        "the guard must refuse before anything is dialled"
    );
}
