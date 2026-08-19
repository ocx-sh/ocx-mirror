// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::*;

fn url(input: &str) -> Url {
    Url::parse(input).expect("test URL parses")
}

// ── netrc lookup ───────────────────────────────────────────────────────────

#[test]
fn reads_the_matching_machine_entry() {
    let text = "machine other.example login wrong password wrong\n\
                machine nexus.corp.example\n  login ci-mirror\n  password s3cr3t\n";
    assert_eq!(
        lookup_netrc(text, "nexus.corp.example"),
        Some(("ci-mirror".to_string(), "s3cr3t".to_string()))
    );
}

/// Newlines carry no meaning in netrc — the whole file is one token stream.
#[test]
fn reads_an_entry_written_on_one_line() {
    let text = "machine nexus.corp.example login ci password s3cr3t\n";
    assert_eq!(
        lookup_netrc(text, "nexus.corp.example"),
        Some(("ci".to_string(), "s3cr3t".to_string()))
    );
}

/// Exact match only: a `machine corp.example` line must never answer for a
/// host that merely ends with it, or an attacker registering
/// `evil-corp.example` would collect the corporate credential.
#[test]
fn does_not_match_a_host_by_suffix() {
    let text = "machine corp.example login ci password s3cr3t\n";
    assert_eq!(lookup_netrc(text, "evil-corp.example"), None);
    assert_eq!(lookup_netrc(text, "sub.corp.example"), None);
}

/// `default` matches every host, so a `default` entry answering here is the
/// operator's credential leaving for whatever host a hostile lock or index
/// named. It never answers.
#[test]
fn default_never_answers() {
    let text = "default login fallback password fallback-secret\n\
                machine nexus.corp.example login ci password s3cr3t\n";
    assert_eq!(
        lookup_netrc(text, "nexus.corp.example"),
        Some(("ci".to_string(), "s3cr3t".to_string())),
        "a `machine` entry still answers for its own host"
    );
    assert_eq!(
        lookup_netrc(text, "evil.example"),
        None,
        "a `default` entry must never answer for a host the operator did not name"
    );
}

/// A `default` block closes the `machine` block before it. Without that, its
/// own `login`/`password` tokens would be attributed to the preceding host —
/// the same leak wearing a different shape.
#[test]
fn a_default_block_does_not_bleed_into_the_preceding_machine() {
    let text = "machine nexus.corp.example login ci\n\
                default password evil\n";
    assert_eq!(lookup_netrc(text, "nexus.corp.example"), None);
}

/// The whole ladder, on the path the attacker actually takes: `$NETRC` carries
/// a `default` entry and a hostile lock names a wheel on a host the operator
/// never configured. Anonymous, not the operator's credential.
#[test]
fn resolve_sends_no_default_credential_to_an_unconfigured_host() {
    let _guard = crate::test_support::ocx_env_lock();

    let dir = tempfile::tempdir().expect("tempdir");
    let netrc = dir.path().join("netrc");
    std::fs::write(&netrc, "default login fallback password fallback-secret\n").expect("write netrc");

    // SAFETY: serialised by the crate-wide env lock held above.
    unsafe {
        std::env::set_var("NETRC", &netrc);
    }

    let resolved = resolve(&url("https://evil.example/pkg.whl"));

    // SAFETY: same lock.
    unsafe {
        std::env::remove_var("NETRC");
    }

    assert_eq!(
        resolved.expect("resolves"),
        None,
        "a netrc `default` entry must not reach a host no spec named"
    );
}

/// An unterminated `macdef` must not swallow the entries after it.
#[test]
fn skips_a_macdef_body_up_to_the_blank_line() {
    let text = "macdef init\n\
                machine not-a-real-entry login nope password nope\n\
                \n\
                machine nexus.corp.example login ci password s3cr3t\n";
    assert_eq!(
        lookup_netrc(text, "nexus.corp.example"),
        Some(("ci".to_string(), "s3cr3t".to_string()))
    );
    assert_eq!(lookup_netrc(text, "not-a-real-entry"), None);
}

#[test]
fn an_entry_without_a_password_is_not_a_credential() {
    let text = "machine nexus.corp.example login ci\n";
    assert_eq!(lookup_netrc(text, "nexus.corp.example"), None);
}

/// Quotes around a value are not part of it. A quoted value containing
/// whitespace is out of scope — the token stream is whitespace-delimited, the
/// same limitation `curl` documents for its own reader.
#[test]
fn strips_quotes_from_a_quoted_value() {
    let text = "machine nexus.corp.example login \"ci\" password \"s3cr3t\"\n";
    assert_eq!(
        lookup_netrc(text, "nexus.corp.example"),
        Some(("ci".to_string(), "s3cr3t".to_string()))
    );
}

// ── environment rung ───────────────────────────────────────────────────────

/// `USER` + `TOKEN` with no declared type is Basic; a lone `TOKEN` is Bearer.
/// Same inference as `ocx_lib::auth::get_env_auth`, so one convention covers
/// registries and package indexes alike.
#[test]
fn env_pair_is_basic_and_a_lone_token_is_bearer() {
    let _guard = crate::test_support::ocx_env_lock();

    // SAFETY: serialised by the crate-wide env lock held above.
    unsafe {
        std::env::set_var("OCX_AUTH_nexus_corp_example_USER", "ci");
        std::env::set_var("OCX_AUTH_nexus_corp_example_TOKEN", "s3cr3t");
        std::env::set_var("OCX_AUTH_bearer_example_TOKEN", "t0ken");
    }

    let basic = from_env("nexus.corp.example").expect("resolves");
    assert_eq!(
        basic,
        Some(Credential::Basic {
            user: "ci".to_string(),
            secret: "s3cr3t".to_string()
        })
    );
    let bearer = from_env("bearer.example").expect("resolves");
    assert_eq!(bearer, Some(Credential::Bearer("t0ken".to_string())));

    // SAFETY: same lock.
    unsafe {
        std::env::remove_var("OCX_AUTH_nexus_corp_example_USER");
        std::env::remove_var("OCX_AUTH_nexus_corp_example_TOKEN");
        std::env::remove_var("OCX_AUTH_bearer_example_TOKEN");
    }
}

/// A declared type whose variables are missing fails the run instead of
/// degrading to anonymous — the operator set it because they meant it.
#[test]
fn a_half_configured_identity_is_a_usage_error() {
    let _guard = crate::test_support::ocx_env_lock();

    // SAFETY: serialised by the crate-wide env lock held above.
    unsafe {
        std::env::set_var("OCX_AUTH_half_example_TYPE", "basic");
        std::env::set_var("OCX_AUTH_half_example_USER", "ci");
    }

    let error = from_env("half.example").expect_err("a declared basic identity needs its token");
    let rendered = error.to_string();
    assert!(
        rendered.contains("OCX_AUTH_half_example_TOKEN"),
        "the error must name the variable to set, got: {rendered}"
    );

    // SAFETY: same lock.
    unsafe {
        std::env::remove_var("OCX_AUTH_half_example_TYPE");
        std::env::remove_var("OCX_AUTH_half_example_USER");
    }
}

/// A URL with no authority (`data:`, `file:`) resolves to anonymous rather
/// than panicking on the missing host.
#[test]
fn a_hostless_url_is_anonymous() {
    assert_eq!(resolve(&url("data:text/plain,hi")).expect("resolves"), None);
}

// ── redaction ──────────────────────────────────────────────────────────────

/// The secret must not reach a log line through `Debug`.
#[test]
fn debug_never_renders_the_secret() {
    let basic = Credential::Basic {
        user: "ci".to_string(),
        secret: "hunter2".to_string(),
    };
    let bearer = Credential::Bearer("hunter2".to_string());
    for rendered in [format!("{basic:?}"), format!("{bearer:?}")] {
        assert!(
            !rendered.contains("hunter2"),
            "credential leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("redacted"), "unexpected rendering: {rendered}");
    }
}

/// A bearer token reaches a Basic-only resolver as the password, under the
/// sentinel user name every Python index documents for tokens.
#[test]
fn a_bearer_token_becomes_a_token_password_pair() {
    assert_eq!(
        Credential::Bearer("t0ken".to_string()).as_basic_pair(),
        ("__token__".to_string(), "t0ken".to_string())
    );
}
