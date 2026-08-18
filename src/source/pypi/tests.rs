// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// Install the rustls crypto provider exactly once per process. Reqwest
/// builds its TLS stack lazily on first use and panics with "no provider
/// set" if none is registered, even for `http://` URLs. Same helper as
/// `pipeline/download.rs`'s test module (not centralized upstream).
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Spawns a local loopback index that answers `responses` in order, one per
/// connection, and returns its `http://127.0.0.1:<port>` base URL plus the
/// request heads it saw. Test-only stand-in for a Simple API index — no
/// external network access.
async fn spawn_index(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut scratch = [0u8; 2048];
            let read = socket.read(&mut scratch).await.unwrap_or(0);
            seen.push(String::from_utf8_lossy(&scratch[..read]).to_string());
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        seen
    });

    (format!("http://{addr}"), server)
}

fn response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn json_ok(body: &str) -> String {
    response("200 OK", "application/vnd.pypi.simple.v1+json", body)
}

fn not_found() -> String {
    response("404 Not Found", "text/plain", "")
}

/// One un-yanked wheel, one fully yanked release, one dev release, and a file
/// this parser has no use for.
const PROJECT_JSON: &str = r#"{
    "meta": {"api-version": "1.0"},
    "name": "pycowsay",
    "files": [
        {"filename": "pycowsay-1.0.0-py3-none-any.whl", "yanked": false},
        {"filename": "pycowsay-1.0.0.tar.gz", "yanked": false},
        {"filename": "pycowsay-1.1.0-py3-none-any.whl", "yanked": true},
        {"filename": "pycowsay-2.0.0.dev0-py3-none-any.whl", "yanked": false},
        {"filename": "pycowsay-1.0.0-py3-none-any.whl.asc", "yanked": false}
    ]
}"#;

// ── discovery ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn drops_versions_whose_files_are_all_yanked() {
    install_crypto_provider();
    let (index, server) = spawn_index(vec![json_ok(PROJECT_JSON)]).await;

    let versions = list_versions("pycowsay", &[index]).await.unwrap();
    server.await.unwrap();

    let mut names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["1.0.0", "2.0.0.dev0"], "yanked 1.1.0 must be dropped");
    assert!(versions.iter().all(|v| v.assets.is_empty()));
}

/// PEP 592: a version stays as long as *some* file of it is installable, so a
/// yanked wheel next to a live sdist is not a dropped version.
#[tokio::test]
async fn keeps_a_version_with_one_yanked_and_one_live_file() {
    install_crypto_provider();
    let body = r#"{"files": [
        {"filename": "pkg-1.0.0-py3-none-any.whl", "yanked": true},
        {"filename": "pkg-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl", "yanked": false}
    ]}"#;
    let (index, server) = spawn_index(vec![json_ok(body)]).await;

    let versions = list_versions("pkg", &[index]).await.unwrap();
    server.await.unwrap();

    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].version, "1.0.0");
}

/// PEP 691 encodes a yank reason as a bare string, which is itself the marker.
#[tokio::test]
async fn treats_a_yank_reason_string_as_yanked() {
    install_crypto_provider();
    let body = r#"{"files": [
        {"filename": "pkg-1.0.0-py3-none-any.whl", "yanked": "broken build"},
        {"filename": "pkg-2.0.0-py3-none-any.whl"}
    ]}"#;
    let (index, server) = spawn_index(vec![json_ok(body)]).await;

    let versions = list_versions("pkg", &[index]).await.unwrap();
    server.await.unwrap();

    let names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
    assert_eq!(names, vec!["2.0.0"], "a reason string yanks; an absent key does not");
}

/// The HTML form is what Artifactory and Nexus serve, so it must produce the
/// same versions as the JSON form — including the `data-yanked` rule.
#[tokio::test]
async fn reads_a_pep503_html_page() {
    install_crypto_provider();
    let html = r#"<!DOCTYPE html><html><body>
        <a href="../../packages/pkg-1.0.0-py3-none-any.whl#sha256=aa">pkg-1.0.0-py3-none-any.whl</a><br/>
        <a href="../../packages/pkg-1.1.0-py3-none-any.whl#sha256=bb" data-yanked="">pkg-1.1.0-py3-none-any.whl</a><br/>
        <a href="../../packages/pkg-1.2.0.tar.gz#sha256=cc">pkg-1.2.0.tar.gz</a>
    </body></html>"#;
    let (index, server) = spawn_index(vec![response("200 OK", "text/html; charset=utf-8", html)]).await;

    let versions = list_versions("pkg", &[index]).await.unwrap();
    server.await.unwrap();

    let mut names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["1.0.0", "1.2.0"], "the yanked anchor must be dropped");
}

#[tokio::test]
async fn flags_pep440_prereleases() {
    install_crypto_provider();
    let (index, server) = spawn_index(vec![json_ok(PROJECT_JSON)]).await;

    let versions = list_versions("pycowsay", &[index]).await.unwrap();
    server.await.unwrap();

    let stable = versions.iter().find(|v| v.version == "1.0.0").unwrap();
    assert!(!stable.is_prerelease);
    let dev = versions.iter().find(|v| v.version == "2.0.0.dev0").unwrap();
    assert!(dev.is_prerelease, "dev release must flag as prerelease");
}

/// BLOCK-tier supply-chain guard, carried over from the JSON-API adapter:
/// filenames are attacker-controlled when the index is hostile. A version
/// string reaching `uv pip compile -` stdin as `{package}=={version}` must not
/// be able to smuggle a second requirement line or a path traversal. Weird but
/// safe schemes still mirror — only dangerous characters are rejected.
#[test]
fn rejects_version_strings_with_injection_or_traversal_characters() {
    let files = vec![
        ("pkg-1.0.0-py3-none-any.whl".to_string(), false),
        (
            "pkg-1.0.1\nevil @ https://attacker.example/evil.whl-py3-none-any.whl".to_string(),
            false,
        ),
        ("pkg-1.0.2/../../../etc-py3-none-any.whl".to_string(), false),
        ("pkg-2024.1.1.post1+local-py3-none-any.whl".to_string(), false),
    ];

    let versions = versions_from_files(files);
    let mut names: Vec<String> = versions.into_iter().map(|v| v.version).collect();
    names.sort();

    assert!(
        names.contains(&"1.0.0".to_string()),
        "safe version must survive: {names:?}"
    );
    assert!(
        names.contains(&"2024.1.1.post1+local".to_string()),
        "an unusual but safe version must survive: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|name| name.contains('\n') || name.contains('/') || name.contains('\\')),
        "no dangerous character may reach downstream consumers: {names:?}"
    );
}

// ── index ordering ─────────────────────────────────────────────────────────

/// First index that *has* the project wins — uv's `first-index` strategy. An
/// index that answers 404 simply does not carry it, so the next one is tried.
#[tokio::test]
async fn falls_through_a_404_to_the_next_index() {
    install_crypto_provider();
    let (absent, absent_server) = spawn_index(vec![not_found()]).await;
    let (present, present_server) = spawn_index(vec![json_ok(PROJECT_JSON)]).await;

    let versions = list_versions("pycowsay", &[absent, present]).await.unwrap();
    absent_server.await.unwrap();
    present_server.await.unwrap();

    assert_eq!(versions.len(), 2, "the second index answers");
}

/// Candidates are never merged across indexes: an internal index that carries
/// the project is the answer, and a public index listing a same-named package
/// is never consulted. That is the dependency-confusion guard.
#[tokio::test]
async fn does_not_merge_candidates_across_indexes() {
    install_crypto_provider();
    let internal = r#"{"files": [{"filename": "pkg-1.0.0-py3-none-any.whl", "yanked": false}]}"#;
    let (first, first_server) = spawn_index(vec![json_ok(internal)]).await;
    let (second, second_server) = spawn_index(vec![json_ok(PROJECT_JSON)]).await;

    let versions = list_versions("pkg", &[first, second]).await.unwrap();
    first_server.await.unwrap();

    let names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
    assert_eq!(names, vec!["1.0.0"], "only the first index that has it is read");
    second_server.abort();
}

/// A 404 everywhere is malformed input (exit 65), not an unavailable source.
#[tokio::test]
async fn absent_from_every_index_is_a_pypi_error() {
    install_crypto_provider();
    let (first, first_server) = spawn_index(vec![not_found()]).await;
    let (second, second_server) = spawn_index(vec![not_found()]).await;

    let error = list_versions("nonexistent-package", &[first, second])
        .await
        .unwrap_err();
    first_server.await.unwrap();
    second_server.await.unwrap();

    let mirror_error = classify_error("failed to list PyPI releases", error);
    assert!(
        matches!(mirror_error, MirrorError::PypiError(_)),
        "got: {mirror_error:?}"
    );
}

/// A 500 means "unknown", not "absent" — it must not silently fall through to
/// the next index and mirror the wrong project.
#[tokio::test]
async fn a_server_error_aborts_instead_of_falling_through() {
    install_crypto_provider();
    let (broken, broken_server) = spawn_index(vec![response("500 Internal Server Error", "text/plain", "")]).await;
    let (other, other_server) = spawn_index(vec![json_ok(PROJECT_JSON)]).await;

    let error = list_versions("pycowsay", &[broken, other]).await.unwrap_err();
    broken_server.await.unwrap();

    let mirror_error = classify_error("failed to list PyPI releases", error);
    assert!(
        matches!(mirror_error, MirrorError::SourceError(_)),
        "got: {mirror_error:?}"
    );
    other_server.abort();
}

#[tokio::test]
async fn classify_error_maps_connection_refused_to_source_error() {
    install_crypto_provider();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // reserved, unused: connection refused, not a timeout

    let error = list_versions("pycowsay", &[format!("http://127.0.0.1:{port}")])
        .await
        .unwrap_err();
    let mirror_error = classify_error("failed to list PyPI releases", error);
    assert!(
        matches!(mirror_error, MirrorError::SourceError(_)),
        "got: {mirror_error:?}"
    );
}

#[tokio::test]
async fn surfaces_an_unparseable_body() {
    install_crypto_provider();
    let (index, server) = spawn_index(vec![json_ok("{not json")]).await;

    let error = list_versions("pycowsay", &[index]).await.unwrap_err();
    server.await.unwrap();

    let mirror_error = classify_error("failed to list PyPI releases", error);
    assert!(
        matches!(mirror_error, MirrorError::SourceError(_)),
        "got: {mirror_error:?}"
    );
}

// ── request shape ──────────────────────────────────────────────────────────

/// The project URL is `{base}/{normalized}/` — the base exactly as configured,
/// with no vendor-specific suffix invented for it, and the name normalized per
/// PEP 503.
#[tokio::test]
async fn requests_the_pep503_project_url_and_negotiates_json() {
    install_crypto_provider();
    let (index, server) = spawn_index(vec![json_ok(r#"{"files": []}"#)]).await;

    list_versions("Flask_Cors.Extra", &[format!("{index}/simple/")])
        .await
        .unwrap();
    let seen = server.await.unwrap();

    let head = seen.first().expect("one request");
    assert!(
        head.starts_with("GET /simple/flask-cors-extra/ "),
        "unexpected request line: {head}"
    );
    assert!(
        head.contains("application/vnd.pypi.simple.v1+json"),
        "the JSON serialization must be negotiated: {head}"
    );
}

/// A credential configured for the index host is attached to discovery, which
/// is what makes an authenticated corporate index reachable at all.
#[tokio::test]
async fn attaches_a_configured_credential_to_discovery() {
    install_crypto_provider();
    let guard = crate::test_support::OCX_ENV_LOCK.lock().await;
    let (index, server) = spawn_index(vec![json_ok(r#"{"files": []}"#)]).await;

    // SAFETY: serialised by the crate-wide env lock held above.
    unsafe {
        std::env::set_var("OCX_AUTH_127_0_0_1_USER", "ci-mirror");
        std::env::set_var("OCX_AUTH_127_0_0_1_TOKEN", "s3cr3t");
    }
    let result = list_versions("pkg", &[index]).await;
    // SAFETY: same lock.
    unsafe {
        std::env::remove_var("OCX_AUTH_127_0_0_1_USER");
        std::env::remove_var("OCX_AUTH_127_0_0_1_TOKEN");
    }
    result.unwrap();
    let seen = server.await.unwrap();
    drop(guard);

    // base64("ci-mirror:s3cr3t"); reqwest writes header names lowercased.
    let head = seen.first().expect("one request").to_lowercase();
    assert!(
        head.contains("authorization: basic y2ktbwlycm9yonmzy3izda=="),
        "credential must be attached: {head}"
    );
}
