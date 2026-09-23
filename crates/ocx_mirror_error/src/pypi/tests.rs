// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Classification of real `source::pypi::list_versions` failures, served by a
//! loopback index. The fixture helpers are copies of the adapter's own test
//! helpers (`source/pypi/tests.rs`): the two test modules sit in different
//! crates, so they cannot share a `#[cfg(test)]` item.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use ocx_mirror_source::pypi::list_versions;
use ocx_mirror_test_support::install_crypto_provider;

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

// ── classification ──

/// A 404 everywhere is malformed input (exit 65), not an unavailable source.
#[tokio::test]
async fn absent_from_every_index_is_a_pypi_error() {
    install_crypto_provider();
    let (first, first_server) = spawn_index(vec![not_found()]).await;
    let (second, second_server) = spawn_index(vec![not_found()]).await;

    let error = list_versions("nonexistent-package", &[first, second.clone()])
        .await
        .unwrap_err();
    first_server.await.unwrap();
    second_server.await.unwrap();

    let mirror_error = classify_error("failed to list PyPI releases", error);
    assert!(
        matches!(mirror_error, MirrorError::PypiError(_)),
        "got: {mirror_error:?}"
    );
    // Exact bytes: pinned before the crate split moves this module (E4). The
    // last index's 404 is the one reported.
    assert_eq!(
        mirror_error.to_string(),
        format!(
            "pypi error: failed to list PyPI releases: HTTP status client error (404 Not Found) for url ({second}/nonexistent-package/)"
        )
    );
    assert_eq!(mirror_error.kind_exit_code(), ocx_exit::ExitCode::DataError);
}

/// A 500 means "unknown", not "absent" — it must not silently fall through to
/// the next index and mirror the wrong project.
#[tokio::test]
async fn a_server_error_aborts_instead_of_falling_through() {
    install_crypto_provider();
    let (broken, broken_server) = spawn_index(vec![response("500 Internal Server Error", "text/plain", "")]).await;
    let (other, other_server) = spawn_index(vec![json_ok(PROJECT_JSON)]).await;

    let error = list_versions("pycowsay", &[broken.clone(), other]).await.unwrap_err();
    broken_server.await.unwrap();

    let mirror_error = classify_error("failed to list PyPI releases", error);
    assert!(
        matches!(mirror_error, MirrorError::SourceError(_)),
        "got: {mirror_error:?}"
    );
    // Exact bytes: pinned before the crate split moves this module (E4).
    assert_eq!(
        mirror_error.to_string(),
        format!(
            "source error: failed to list PyPI releases: HTTP status server error (500 Internal Server Error) for url ({broken}/pycowsay/)"
        )
    );
    assert_eq!(mirror_error.kind_exit_code(), ocx_exit::ExitCode::Unavailable);
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
