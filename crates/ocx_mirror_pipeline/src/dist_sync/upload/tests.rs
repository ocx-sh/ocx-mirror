// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::BTreeMap;

use super::*;

fn composed(base: &str, relative: &str) -> String {
    super::super::mirrored_url(&Url::parse(base).expect("test base URL must parse"), relative)
        .expect("composition must succeed")
        .to_string()
}

#[test]
fn a_base_url_with_a_path_keeps_it() {
    // Artifactory and GitLab both put the whole repository behind a path, so
    // composing onto the origin would upload into the wrong repository.
    assert_eq!(
        composed("https://art.test/artifactory/ocx-dist", "v0.5.8/ocx.tar.gz"),
        "https://art.test/artifactory/ocx-dist/v0.5.8/ocx.tar.gz"
    );
}

#[test]
fn a_trailing_slash_on_the_base_does_not_double_the_separator() {
    assert_eq!(
        composed("https://art.test/artifactory/ocx-dist/", "dist.json"),
        "https://art.test/artifactory/ocx-dist/dist.json"
    );
}

#[test]
fn a_nested_relative_path_becomes_nested_url_segments() {
    assert_eq!(
        composed("https://art.test/ocx", "dist/abc123.json"),
        "https://art.test/ocx/dist/abc123.json"
    );
}

/// `filename` is foreign data, so a reserved character in it is not something
/// validation rules out — and the upload target and the `url` stamped into the
/// manifest must still name the same object. They did not when they were two
/// functions: one escaped, one concatenated, and the store held bytes the
/// manifest pointed past. One composition is what makes this hold, so the case
/// that used to diverge is what pins it.
#[test]
fn a_filename_needing_escape_composes_to_one_escaped_url() {
    let base = "https://gitlab.test/api/v4/projects/42/packages/generic/ocx";

    assert_eq!(
        composed(base, "0.5.8/ocx build.tar.gz"),
        "https://gitlab.test/api/v4/projects/42/packages/generic/ocx/0.5.8/ocx%20build.tar.gz"
    );
    assert_eq!(
        composed(base, "0.5.8/ocx#next.tar.gz"),
        "https://gitlab.test/api/v4/projects/42/packages/generic/ocx/0.5.8/ocx%23next.tar.gz"
    );
}

/// A retry cannot fix a credential problem, and hammering one burns the
/// backoff window and trips account lockout policy on exactly the stores this
/// targets.
#[test]
fn a_4xx_is_never_retried() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::CONFLICT,
    ] {
        assert!(!retryable_status(status), "{status} must not be retried");
    }
}

#[test]
fn a_5xx_and_a_429_are_retried() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        assert!(retryable_status(status), "{status} must be retried");
    }
}

#[test]
fn retry_after_in_seconds_is_honoured_when_it_exceeds_the_schedule() {
    let mut headers = HeaderMap::new();
    headers.insert(RETRY_AFTER, HeaderValue::from_static("30"));

    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));
    assert_eq!(
        effective_delay(Duration::from_secs(5), Some(Duration::from_secs(30))),
        Duration::from_secs(30)
    );
}

#[test]
fn a_retry_after_shorter_than_the_schedule_does_not_shorten_the_wait() {
    assert_eq!(
        effective_delay(Duration::from_secs(30), Some(Duration::from_secs(1))),
        Duration::from_secs(30),
        "the schedule is a floor; a server asking for less does not get the run hammering it"
    );
}

/// Without the ceiling, five steps against a store answering
/// `Retry-After: 3600` is a five-hour hang in CI.
#[test]
fn a_large_retry_after_is_clamped() {
    assert_eq!(
        effective_delay(Duration::from_secs(1), Some(Duration::from_secs(3600))),
        RETRY_AFTER_CEILING
    );
}

#[test]
fn an_http_date_retry_after_falls_back_to_the_schedule() {
    // Parsing the date form needs a trusted clock on both ends, and the
    // scheduled backoff is already a correct answer.
    let mut headers = HeaderMap::new();
    headers.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"));

    assert_eq!(parse_retry_after(&headers), None);
    assert_eq!(effective_delay(Duration::from_secs(5), None), Duration::from_secs(5));
}

#[test]
fn an_absent_retry_after_leaves_the_schedule_alone() {
    assert_eq!(parse_retry_after(&HeaderMap::new()), None);
}

/// Every string here reaches a CI log that outlives the run.
#[test]
fn a_credential_never_reaches_a_debug_line() {
    let bearer = ResolvedIdentity::Bearer {
        token: "hunter2".to_string(),
    };
    let basic = ResolvedIdentity::Basic {
        username: "ci".to_string(),
        password: "hunter2".to_string(),
    };

    for rendered in [format!("{bearer:?}"), format!("{basic:?}")] {
        assert!(!rendered.contains("hunter2"), "a secret leaked into Debug: {rendered}");
    }
}

#[test]
fn userinfo_is_stripped_from_a_logged_url() {
    let url = Url::parse("https://ci:hunter2@art.test/ocx/dist.json").expect("test URL must parse");

    let rendered = redacted(&url);

    assert!(
        !rendered.contains("hunter2"),
        "a password leaked into a log line: {rendered}"
    );
}

#[test]
fn an_unset_environment_variable_is_named_but_never_read_back() {
    let error = read_env("OCX_MIRROR_TEST_DEFINITELY_UNSET").expect_err("an unset variable must fail");

    assert!(error.to_string().contains("OCX_MIRROR_TEST_DEFINITELY_UNSET"));
}

#[test]
fn a_bad_header_name_is_refused_at_construction() {
    let mut headers = BTreeMap::new();
    headers.insert("not a header name".to_string(), "value".to_string());

    assert!(
        build_headers(&headers).is_err(),
        "a malformed header must fail before the run rather than during it"
    );
}

#[test]
fn a_configured_header_is_carried_verbatim() {
    // The escape hatch that keeps one PUT implementation covering Azure Blob.
    let mut configured = BTreeMap::new();
    configured.insert("x-ms-blob-type".to_string(), "BlockBlob".to_string());

    let headers = build_headers(&configured).expect("a well-formed header must build");

    assert_eq!(
        headers.get("x-ms-blob-type").and_then(|value| value.to_str().ok()),
        Some("BlockBlob")
    );
}

/// Known vectors for the empty input, from each algorithm's own spec. Locks the
/// four hex encodings against a silent swap of one hasher for another — the
/// failure mode is a header Artifactory rejects, or worse, silently records
/// against the wrong algorithm.
#[test]
fn the_four_checksums_match_their_published_vectors() {
    let sums = Checksums::of(b"");

    assert_eq!(sums.md5, "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(sums.sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(
        sums.sha256,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sums.sha512,
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
         47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
    );
}

/// Lowercase hex without an algorithm prefix — the `X-Checksum-*` grammar. A
/// `sha256:`-prefixed value is what the manifest carries, and sending that
/// shape would be rejected.
#[test]
fn checksums_are_bare_lowercase_hex() {
    let sums = Checksums::of(b"ocx");

    for value in [&sums.md5, &sums.sha1, &sums.sha256, &sums.sha512] {
        assert!(
            value.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "not bare lowercase hex: {value}"
        );
    }
    assert_eq!(sums.md5.len(), 32);
    assert_eq!(sums.sha1.len(), 40);
    assert_eq!(sums.sha256.len(), 64);
    assert_eq!(sums.sha512.len(), 128);
}
