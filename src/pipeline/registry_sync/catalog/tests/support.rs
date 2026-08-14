// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! A canned-response HTTP server on loopback, plus the fixtures every catalog
//! test topic shares.
//!
//! Hand-rolled over `tokio::net::TcpListener` rather than pulled in as a mock
//! crate: the tests need three things no HTTP client library exposes — a count
//! of the TCP connections that were *opened* (the SSRF-before-any-request
//! assertion), a response with a deliberately lying `Content-Length`, and a
//! chunked body with no length at all. That is ~60 lines against a new
//! dev-dependency.
//!
//! Every response carries `Connection: close` and the connection is dropped
//! after it, so one connection is exactly one request and the two counters
//! cannot drift.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A `sha256:` digest of the right shape for [`RootTag::content`], so a root
/// fixture deserializes through `oci::Digest`'s exact-wire serde.
pub const FIXTURE_DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

/// `trusted_hosts` opening the loopback address the test server binds — the
/// same escape hatch a corporate index on an RFC1918 address uses.
pub fn loopback_trusted() -> Vec<String> {
    vec!["127.0.0.1".to_string()]
}

/// Install the rustls crypto provider exactly once per process. Reqwest builds
/// its TLS stack on `ClientBuilder::build` and panics with "no provider set"
/// if none is registered, even for `http://` URLs. Same helper as
/// `pipeline/download.rs`'s and `source/pypi.rs`'s test modules (not
/// centralized upstream).
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// One canned response, keyed by the request path it answers.
pub struct Route {
    path: String,
    status: String,
    headers: Vec<String>,
    body: String,
    chunked: bool,
}

impl Route {
    /// `200 OK` with `body` served verbatim as JSON.
    pub fn json(path: &str, body: &str) -> Route {
        Route {
            path: path.to_string(),
            status: "200 OK".to_string(),
            headers: Vec::new(),
            body: body.to_string(),
            chunked: false,
        }
    }

    /// A bare status with an empty body — `404 Not Found`, `304 Not Modified`,
    /// `500 Internal Server Error`.
    pub fn status(path: &str, status: &str) -> Route {
        Route {
            path: path.to_string(),
            status: status.to_string(),
            headers: Vec::new(),
            body: String::new(),
            chunked: false,
        }
    }

    /// `302 Found` pointing at `location` — the redirect the index client must
    /// not follow.
    pub fn redirect(path: &str, location: &str) -> Route {
        Route {
            path: path.to_string(),
            status: "302 Found".to_string(),
            headers: vec![format!("Location: {location}")],
            body: String::new(),
            chunked: false,
        }
    }

    /// `200 OK` whose `Content-Length` claims `declared` bytes regardless of
    /// what the body actually holds — the declared-oversize refusal path.
    pub fn declared_length(path: &str, body: &str, declared: usize) -> Route {
        Route {
            path: path.to_string(),
            status: "200 OK".to_string(),
            headers: vec![format!("Content-Length: {declared}")],
            body: body.to_string(),
            chunked: false,
        }
    }

    /// `200 OK` with a chunked body and **no** `Content-Length` — the case the
    /// declared-size check cannot see and the streaming cap must catch.
    pub fn chunked(path: &str, body: &str) -> Route {
        Route {
            path: path.to_string(),
            status: "200 OK".to_string(),
            headers: Vec::new(),
            body: body.to_string(),
            chunked: true,
        }
    }

    fn render(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {}\r\nConnection: close\r\n", self.status);
        for header in &self.headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        let body = if self.chunked {
            head.push_str("Transfer-Encoding: chunked\r\n");
            format!("{:x}\r\n{}\r\n0\r\n\r\n", self.body.len(), self.body)
        } else {
            if !self.headers.iter().any(|header| header.starts_with("Content-Length:")) {
                head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
            }
            self.body.clone()
        };
        head.push_str("\r\n");
        head.push_str(&body);
        head.into_bytes()
    }
}

/// A loopback HTTP server that answers a fixed route table and records every
/// connection it accepts and every path it was asked for.
pub struct TestIndex {
    address: SocketAddr,
    connections: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
}

impl TestIndex {
    /// Bind an ephemeral loopback port and serve `routes`; an unrouted path
    /// answers `404`.
    pub async fn start(routes: Vec<Route>) -> TestIndex {
        install_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        let connections = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));

        let served = Arc::new(routes);
        let accepted = Arc::clone(&connections);
        let recorded = Arc::clone(&paths);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);

                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while let Ok(read) = stream.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                if request.is_empty() {
                    continue;
                }

                let head = String::from_utf8_lossy(&request).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
                recorded.lock().expect("path recorder").push(path.clone());

                let response = served
                    .iter()
                    .find(|route| route.path == path)
                    .map_or_else(|| Route::status(&path, "404 Not Found").render(), Route::render);
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            }
        });

        TestIndex {
            address,
            connections,
            paths,
        }
    }

    /// The `http://127.0.0.1:<port>` base every fetch is minted from.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The bound address, for a pointer that names this server by authority.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// How many TCP connections were accepted — the recorder that makes
    /// "refused before any request was issued" an observation, not a claim.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Every request path served, in order.
    pub fn paths(&self) -> Vec<String> {
        self.paths.lock().expect("path recorder").clone()
    }

    /// Give an in-flight connection a moment to land before asserting a count,
    /// so a zero reads as "nothing was dialled" and not "the accept loop has
    /// not been polled yet".
    pub async fn settle(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
