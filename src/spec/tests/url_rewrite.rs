// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `source.url_rewrite` and its `OCX_MIRROR_URL_REWRITE` override (#75).

use super::super::*;
use crate::error::MirrorError;
use crate::test_support::{EnvRestore, ocx_env_lock};
use url::Url;

const ARTIFACTORY: &str = "https://artifactory.example.com/artifactory/githubcom-remote/";

fn spec_with_source(source: &str) -> Result<MirrorSpec, serde_yaml_ng::Error> {
    serde_yaml_ng::from_str(&format!(
        r#"
name: test-tool
target:
  registry: ocx.sh
  repository: test-tool
source:
{source}
assets:
  linux/amd64:
    - "x\\.tar\\.gz"
"#
    ))
}

fn rewrite(from: &str, to: &str) -> UrlRewrite {
    UrlRewrite {
        from: from.to_string(),
        to: to.to_string(),
    }
}

/// The rewrite as `apply` actually sees it — through the resolve funnel every
/// caller goes through, which is where both halves are normalised.
fn resolved(from: &str, to: &str) -> UrlRewrite {
    let _lock = ocx_env_lock();
    let _env = EnvRestore::set(&[("OCX_MIRROR_URL_REWRITE", None)]);
    UrlRewrite::resolve(Some(&rewrite(from, to)))
        .expect("both halves are valid")
        .expect("the spec's own block")
}

#[test]
fn parses_url_rewrite_on_github_release() {
    let spec = spec_with_source(
        r#"  type: github_release
  owner: Kitware
  repo: CMake
  url_rewrite:
    from: "https://github.com/"
    to: "https://artifactory.example.com/artifactory/githubcom-remote/""#,
    )
    .expect("spec must parse");

    assert_eq!(
        spec.source.url_rewrite(),
        Some(&rewrite("https://github.com/", ARTIFACTORY))
    );
    assert!(spec.validate(std::path::Path::new("mirror.yml")).is_empty());
}

#[test]
fn parses_url_rewrite_on_url_index() {
    // The newtype variant reaches the field through `UrlIndexSourceRaw`, so it
    // needs its own proof that the `type` tag is stripped before the content.
    let spec = spec_with_source(
        r#"  type: url_index
  url: "https://example.com/versions.json"
  url_rewrite:
    from: "https://example.com/"
    to: "https://mirror.internal/example/""#,
    )
    .expect("spec must parse");

    assert_eq!(
        spec.source.url_rewrite(),
        Some(&rewrite("https://example.com/", "https://mirror.internal/example/"))
    );
}

#[test]
fn apply_rewrites_matching_prefix() {
    let r = rewrite("https://github.com/", ARTIFACTORY);
    let url = Url::parse("https://github.com/Kitware/CMake/releases/download/v3.29.0/cmake.tar.gz").unwrap();

    assert_eq!(
        r.apply(&url).unwrap().as_str(),
        "https://artifactory.example.com/artifactory/githubcom-remote/Kitware/CMake/releases/download/v3.29.0/cmake.tar.gz"
    );
}

#[test]
fn apply_leaves_non_matching_url_unchanged() {
    // A release routinely hosts one asset on a CDN the proxy does not front.
    let r = rewrite("https://github.com/", ARTIFACTORY);
    let url = Url::parse("https://cdn.example.org/cmake.tar.gz").unwrap();

    assert_eq!(r.apply(&url).unwrap(), url);
}

#[test]
fn env_override_wins_over_spec() {
    let _lock = ocx_env_lock();
    let _env = EnvRestore::set(&[(
        "OCX_MIRROR_URL_REWRITE",
        Some("https://github.com/=https://mirror.internal/gh/"),
    )]);

    let spec = rewrite("https://github.com/", ARTIFACTORY);
    let resolved = UrlRewrite::resolve(Some(&spec))
        .unwrap()
        .expect("a rewrite is in force");

    // Replaced entirely, not merged: one spec has to serve both a public and
    // an internal deployment byte-identically.
    assert_eq!(resolved.to, "https://mirror.internal/gh/");
}

#[test]
fn env_resolves_without_a_spec_block() {
    let _lock = ocx_env_lock();
    let _env = EnvRestore::set(&[(
        "OCX_MIRROR_URL_REWRITE",
        Some("https://github.com/=https://mirror.internal/gh/"),
    )]);

    let resolved = UrlRewrite::resolve(None).unwrap().expect("a rewrite is in force");
    assert_eq!(resolved.from, "https://github.com/");
}

#[test]
fn an_unset_or_empty_env_leaves_the_spec_in_force() {
    let _lock = ocx_env_lock();
    let spec = rewrite("https://github.com/", ARTIFACTORY);

    for value in [None, Some("")] {
        let _env = EnvRestore::set(&[("OCX_MIRROR_URL_REWRITE", value)]);
        let resolved = UrlRewrite::resolve(Some(&spec)).unwrap().expect("the spec's own block");
        assert_eq!(resolved.to, ARTIFACTORY, "value {value:?}");
    }

    let _env = EnvRestore::set(&[("OCX_MIRROR_URL_REWRITE", None)]);
    assert!(UrlRewrite::resolve(None).unwrap().is_none(), "no spec, no variable");
}

#[test]
fn env_without_equals_is_refused() {
    let _lock = ocx_env_lock();
    let _env = EnvRestore::set(&[("OCX_MIRROR_URL_REWRITE", Some("nonsense"))]);

    let err = UrlRewrite::resolve(None).unwrap_err();
    assert!(matches!(err, MirrorError::SpecUsageError(_)), "{err:?}");
    let message = format!("{err:?}");
    assert!(message.contains("OCX_MIRROR_URL_REWRITE"), "{message}");
    assert!(
        !message.contains("nonsense"),
        "the value must never be echoed: {message}"
    );
}

#[test]
fn env_empty_from_is_refused() {
    let _lock = ocx_env_lock();
    let _env = EnvRestore::set(&[("OCX_MIRROR_URL_REWRITE", Some("=https://mirror.internal/gh/"))]);

    let err = format!("{:?}", UrlRewrite::resolve(None).unwrap_err());
    assert!(err.contains("OCX_MIRROR_URL_REWRITE"), "{err}");
    assert!(err.contains("from must not be empty"), "{err}");
}

#[test]
fn env_non_http_target_is_refused_without_echoing_it() {
    // Same rules as the spec form — an operator variable is not a bypass.
    let _lock = ocx_env_lock();
    let _env = EnvRestore::set(&[("OCX_MIRROR_URL_REWRITE", Some("https://github.com/=file:///etc/passwd"))]);

    let err = format!("{:?}", UrlRewrite::resolve(None).unwrap_err());
    assert!(err.contains("to must be an http(s) URL prefix"), "{err}");
    assert!(!err.contains("passwd"), "the value must never be echoed: {err}");
}

#[test]
fn url_rewrite_on_an_env_source_is_refused_by_name() {
    // Accepted-and-inert would be a lie: an env source's `VersionInfo.assets`
    // is empty by construction, so nothing would ever be rewritten.
    let spec = spec_with_source(
        r#"  type: pypi
  package: pycowsay
  url_rewrite:
    from: "https://files.pythonhosted.org/"
    to: "https://mirror.internal/pypi/""#,
    )
    .expect("the field parses on every variant — the refusal is at validation");

    let errors = spec.validate(std::path::Path::new("mirror.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("not supported for source.type 'pypi'") && e.contains("source.indexes")),
        "{errors:?}"
    );
}

// ── W2: a non-normalised half must not become a silent no-op ──────────────

#[test]
fn a_non_normalised_from_still_matches() {
    // Each of these names github.com and validates clean, but `Url::as_str()`
    // is always in WHATWG form — so left as typed, `strip_prefix` matches
    // nothing, every download goes to the origin, and the proxy the feature
    // exists to route through is bypassed with no error and no log line.
    let url = Url::parse("https://github.com/Kitware/CMake/releases/download/v3.29.0/cmake.tar.gz").unwrap();

    for from in [
        "https://GitHub.com/",
        "HTTPS://github.com/",
        "https://github.com:443/",
        "https://github.com",
    ] {
        let rewritten = resolved(from, ARTIFACTORY).apply(&url).expect("apply succeeds");
        assert_eq!(
            rewritten.as_str(),
            "https://artifactory.example.com/artifactory/githubcom-remote/Kitware/CMake/releases/download/v3.29.0/cmake.tar.gz",
            "from {from:?} must match the normalised upstream URL",
        );
    }
}

#[test]
fn a_to_with_no_path_gains_one() {
    // `https://proxy.internal` parses to `https://proxy.internal/`, which is
    // what terminates the authority before the remainder is concatenated.
    let r = resolved("https://github.com/", "https://proxy.internal");
    assert_eq!(r.to, "https://proxy.internal/");
    assert_eq!(r.from, "https://github.com/");
}

// ── S1: a crafted upstream URL must not move the host ─────────────────────

#[test]
fn a_crafted_upstream_url_cannot_escape_the_proxy_host() {
    // Concatenating a raw `to` that ended at the authority let the remainder
    // extend or replace the host: `https://proxy.internal` + `@evil.example/x`
    // dials evil.example, and `.evil.example` dials a subdomain of it.
    let r = resolved("https://github.com", "https://proxy.internal");

    for crafted in [
        "https://github.com@evil.example/payload.tar.gz",
        "https://github.com.evil.example/payload.tar.gz",
    ] {
        let url = Url::parse(crafted).expect("the crafted URL parses");
        let rewritten = r.apply(&url).expect("apply succeeds");
        assert_eq!(
            rewritten, url,
            "a URL that only looks like the prefix must pass through untouched",
        );
        assert_ne!(
            rewritten.host_str(),
            Some("proxy.internal"),
            "and must never be dressed up as the proxy",
        );
    }
}

// ── W3: a credential must not live in a committed spec, in either half ────

#[test]
fn userinfo_in_from_is_refused_too() {
    // `from` is matched, never dialled — but the rule is that a credential
    // must not be in a committed `mirror.yml` at all, and it is enforced in
    // three other places. Guaranteed inert on top of that: it never matches.
    let spec = spec_with_source(
        r#"  type: github_release
  owner: Kitware
  repo: CMake
  url_rewrite:
    from: "https://user:pass@github.com/"
    to: "https://artifactory.example.com/artifactory/githubcom-remote/""#,
    )
    .expect("spec must parse");

    let errors = spec.validate(std::path::Path::new("mirror.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e == "source.url_rewrite.from must not embed credentials; set OCX_AUTH_<slug>_TOKEN in the environment instead"),
        "{errors:?}"
    );
}

#[test]
fn env_userinfo_in_from_is_refused_without_echoing_it() {
    let _lock = ocx_env_lock();
    let _env = EnvRestore::set(&[(
        "OCX_MIRROR_URL_REWRITE",
        Some("https://user:hunter2@github.com/=https://mirror.internal/gh/"),
    )]);

    let err = format!("{:?}", UrlRewrite::resolve(None).unwrap_err());
    assert!(err.contains("from must not embed credentials"), "{err}");
    assert!(!err.contains("hunter2"), "the value must never be echoed: {err}");
}
