// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use regex::Regex;

use super::VersionInfo;
use crate::pipeline::ocx_cli::push::jitter;

/// GitHub's public API root — the base the injected transport resolves
/// octocrab's relative routes against.
const GITHUB_API_ROOT: &str = "https://api.github.com";

/// Sent on every listing request. GitHub refuses a request without one.
const USER_AGENT: &str = concat!("ocx-mirror/", env!("CARGO_PKG_VERSION"));

/// Deadline for one listing request, headers and body together.
///
/// `crate::http::client()` carries the connect bound and nothing else, by
/// design — its doc comment hands the request deadline to the caller, and the
/// download, catalog, dist and webhook legs each set their own. This leg had
/// none: a proxy or an upstream that accepts the socket and then stalls the
/// body parked `list_upstream_versions` forever, and [`LIST_RETRIES`] never
/// fired, because a hang raises no error to retry. 30s matches
/// `spec::value_source`, the other leg that reads a small JSON document.
const LIST_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Extra attempts a transient page fetch is granted on top of the first.
///
/// Fixed rather than read from `concurrency.max_retries`, which the spec
/// documents as the *push* budget: a push attempt costs minutes, a page fetch
/// milliseconds, and the two ladders have nothing to share but shape.
const LIST_RETRIES: u32 = 5;

/// First retry delay; each further attempt doubles it, capped at
/// [`LIST_RETRY_BACKOFF_MAX`]. Five retries sum to ~31s, which outlasts an API
/// blip and gives up well inside a GitHub incident — the cron reruns anyway.
const LIST_RETRY_BACKOFF_BASE: Duration = Duration::from_secs(1);
const LIST_RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The mirror's own HTTP stack, shaped as the [`tower_service::Service`]
/// octocrab accepts through `OctocrabBuilder::with_service`.
///
/// octocrab's bundled client is a bare `hyper_util` legacy client: it reads no
/// `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`, and it trusts the platform store
/// alone, so `OCX_EXTRA_CA_CERTS` never reached this one leg either. Both were
/// standing, documented exceptions until this transport put release listing on
/// the same `crate::http` factory as every other client the mirror builds.
///
/// octocrab hands the service a **relative** URI — the base URI it would
/// otherwise apply is a tower layer on its config path, and that path is
/// mutually exclusive with `with_service`. So the base lives here, which is
/// also what lets a test point the client at a loopback fixture.
struct MirrorTransport {
    client: reqwest::Client,
    /// Scheme and authority every relative route is resolved against.
    base: http::Uri,
    /// `Authorization` header value, when a `GITHUB_TOKEN` was supplied.
    ///
    /// Carried here rather than through `AuthState`: the service path only
    /// builds under octocrab's `NoConfig`, whose `AuthState` needs a
    /// `secrecy::SecretString` the mirror would otherwise have no reason to
    /// depend on. One header, set in one place.
    authorization: Option<http::HeaderValue>,
}

/// The erased error every failure on this transport arrives as. octocrab
/// requires `Into<tower::BoxError>`, and the leg has two unrelated failure
/// sources — URI composition and reqwest — with no common concrete type.
type TransportError = Box<dyn std::error::Error + Send + Sync>;

impl MirrorTransport {
    /// Resolve one of octocrab's relative routes against [`Self::base`].
    ///
    /// An absolute URI is returned unchanged: nothing in this crate sends one,
    /// but rewriting an authority someone else set would be the wrong answer
    /// if that ever changes.
    fn resolve(&self, uri: http::Uri) -> Result<http::Uri, TransportError> {
        if uri.authority().is_some() {
            return Ok(uri);
        }
        let mut parts = self.base.clone().into_parts();
        parts.path_and_query = Some(
            uri.path_and_query()
                .cloned()
                .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/")),
        );
        Ok(http::Uri::from_parts(parts)?)
    }
}

impl tower_service::Service<http::Request<octocrab::OctoBody>> for MirrorTransport {
    type Response = http::Response<reqwest::Body>;
    type Error = TransportError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // `reqwest::Client` is always ready; it does its own pooling.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<octocrab::OctoBody>) -> Self::Future {
        let client = self.client.clone();
        let resolved = self.resolve(request.uri().clone());
        let authorization = self.authorization.clone();

        Box::pin(async move {
            let (mut parts, body) = request.into_parts();
            parts.uri = resolved?;
            parts
                .headers
                .insert(http::header::USER_AGENT, http::HeaderValue::from_static(USER_AGENT));
            if let Some(value) = authorization {
                parts.headers.insert(http::header::AUTHORIZATION, value);
            }

            let request = http::Request::from_parts(parts, reqwest::Body::wrap(body));
            let response = client.execute(reqwest::Request::try_from(request)?).await?;
            Ok(http::Response::from(response))
        })
    }
}

/// A GitHub listing client on the mirror's own HTTP stack.
///
/// `base` is the API root — [`GITHUB_API_ROOT`] in production, a fixture's
/// address in a test. `token` is `GITHUB_TOKEN`, which only raises the rate
/// limit; listing works unauthenticated.
///
/// octocrab's own retry layer never enters the picture: the service path
/// carries no middleware, so [`list_versions`]'s ladder is the only one. That
/// was already the intent — its default `Simple(3)` fires three retries
/// back-to-back with no delay, which any blip longer than a round-trip
/// defeats, and stacking the two would multiply requests during an outage.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`](crate::error::MirrorError::ExecutionFailed)
/// when the TLS backend cannot be built, or when `base` or `token` cannot be
/// put on the wire.
///
/// # Panics
///
/// Outside a Tokio runtime: octocrab buffers the service, and `tower`'s
/// `Buffer` spawns its worker on construction.
pub(crate) fn client(base: &str, token: Option<&str>) -> Result<octocrab::Octocrab, crate::error::MirrorError> {
    let failed = |message: String| crate::error::MirrorError::ExecutionFailed(vec![message]);

    let parsed: http::Uri = base
        .parse()
        .map_err(|error| failed(format!("invalid GitHub API root '{base}': {error}")))?;
    // A root without both halves composes a relative URI that reqwest refuses
    // at send time — five retries and half a minute after the real mistake.
    if parsed.scheme().is_none() || parsed.authority().is_none() {
        return Err(failed(format!(
            "invalid GitHub API root '{base}': expected a scheme and a host, as in https://api.github.com"
        )));
    }
    let base = parsed;
    let authorization = token
        .map(|token| {
            http::HeaderValue::from_str(&format!("Bearer {token}"))
                .map(|mut value| {
                    value.set_sensitive(true);
                    value
                })
                // Never echo the value: this is a credential.
                .map_err(|_| failed("GITHUB_TOKEN is not a valid HTTP header value".to_string()))
        })
        .transpose()?;

    let transport = MirrorTransport {
        client: crate::http::builder()
            .timeout(LIST_REQUEST_TIMEOUT)
            .build()
            .map_err(|error| failed(format!("cannot build an HTTP client: {error}")))?,
        base,
        authorization,
    };

    // `with_auth(AuthState::None)` is not a choice: `build()` on the service
    // path is implemented for the concrete `AuthState` type, so the builder has
    // to carry one. The `Authorization` header is the transport's.
    Ok(octocrab::OctocrabBuilder::new_empty()
        .with_service(transport)
        .with_auth(octocrab::AuthState::None)
        .build()
        .unwrap_or_else(|infallible| match infallible {}))
}

/// The production client: [`client`] against [`GITHUB_API_ROOT`].
///
/// # Errors
///
/// As [`client`].
pub(crate) fn api_client(token: Option<&str>) -> Result<octocrab::Octocrab, crate::error::MirrorError> {
    client(GITHUB_API_ROOT, token)
}

/// Whether a failed page fetch is one a retry can plausibly clear: a GitHub
/// 5xx or 429, or a transport fault before any status arrived.
///
/// A 403 stays terminal. It is either a real denial or a primary rate limit
/// whose reset is up to an hour away — neither is inside this ladder's reach,
/// and retrying a denial only delays the message.
fn is_transient(error: &octocrab::Error) -> bool {
    match error {
        octocrab::Error::GitHub { source, .. } => {
            source.status_code.is_server_error() || source.status_code.as_u16() == 429
        }
        octocrab::Error::Hyper { .. } | octocrab::Error::Service { .. } => true,
        _ => false,
    }
}

/// `octocrab::Error::GitHub` displays as the bare word `GitHub` — the status
/// and message live one level down. Surface them; every other variant already
/// names its cause.
fn describe(error: &octocrab::Error) -> String {
    match error {
        octocrab::Error::GitHub { source, .. } => format!("GitHub API {}: {}", source.status_code, source.message),
        other => format!("{other}"),
    }
}

/// Delay before attempt `attempt + 1`.
fn retry_delay(attempt: u32) -> Duration {
    let delay = LIST_RETRY_BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(LIST_RETRY_BACKOFF_MAX);
    let delay = jitter(delay);
    // Same scaling as the push ladder: the retry test drives the real loop.
    #[cfg(test)]
    let delay = delay / 1000;
    delay
}

/// One page of releases, retried through the ladder on a transient fault.
async fn fetch_page(
    octocrab: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    page: u32,
) -> anyhow::Result<octocrab::Page<octocrab::models::repos::Release>> {
    let mut attempt = 0u32;
    loop {
        let result = octocrab
            .repos(owner, repo)
            .releases()
            .list()
            .per_page(100)
            .page(page)
            .send()
            .await;
        match result {
            Ok(releases) => return Ok(releases),
            Err(error) if attempt < LIST_RETRIES && is_transient(&error) => {
                attempt += 1;
                let delay = retry_delay(attempt);
                log::warn!(
                    "listing {owner}/{repo} releases page {page}: {}; retry {attempt}/{LIST_RETRIES} in {delay:?}",
                    describe(&error)
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(anyhow::anyhow!("{}", describe(&error))),
        }
    }
}

/// Parse a GitHub release into a `VersionInfo`, if its tag matches the pattern.
/// Returns `None` for drafts or non-matching tags.
fn parse_release(tag_pattern: &Regex, release: &octocrab::models::repos::Release) -> Option<VersionInfo> {
    if release.draft {
        return None;
    }

    let tag = release.tag_name.as_str();
    let captures = tag_pattern.captures(tag)?;

    let version = captures.name("version")?.as_str().to_string();
    let prerelease_suffix = captures.name("prerelease").map(|m| m.as_str().to_string());

    let full_version = match &prerelease_suffix {
        Some(pre) => format!("{version}-{pre}"),
        None => version,
    };

    let mut assets = HashMap::new();
    let mut asset_digests = HashMap::new();
    for asset in &release.assets {
        assets.insert(asset.name.clone(), asset.browser_download_url.clone());
        // GitHub returns `sha256:<hex>` and omits the key on releases
        // published before it added the field; serde resolves the absence to
        // `None`, so an older release simply declares nothing.
        if let Some(digest) = &asset.digest {
            asset_digests.insert(asset.name.clone(), digest.clone());
        }
    }

    Some(VersionInfo {
        version: full_version,
        assets,
        asset_digests,
        is_prerelease: release.prerelease,
    })
}

/// List all versions from GitHub Releases, paginating through all pages.
pub async fn list_versions(
    octocrab: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    tag_pattern: &Regex,
    rate_limit_ms: u64,
) -> anyhow::Result<Vec<VersionInfo>> {
    let mut versions = Vec::new();
    let mut page = 1u32;

    loop {
        let releases = fetch_page(octocrab, owner, repo, page).await?;

        let items = releases.items;
        if items.is_empty() {
            break;
        }

        for release in &items {
            if let Some(version_info) = parse_release(tag_pattern, release) {
                versions.push(version_info);
            }
        }

        if releases.next.is_none() {
            break;
        }

        page += 1;

        if rate_limit_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(rate_limit_ms)).await;
        }
    }

    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pattern(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap()
    }

    fn author_json() -> serde_json::Value {
        serde_json::json!({
            "login": "user",
            "id": 1,
            "node_id": "MDQ6",
            "avatar_url": "https://avatars.githubusercontent.com/u/1",
            "gravatar_id": "",
            "url": "https://api.github.com/users/user",
            "html_url": "https://github.com/user",
            "followers_url": "https://api.github.com/users/user/followers",
            "following_url": "https://api.github.com/users/user/following{/other_user}",
            "gists_url": "https://api.github.com/users/user/gists{/gist_id}",
            "starred_url": "https://api.github.com/users/user/starred{/owner}{/repo}",
            "subscriptions_url": "https://api.github.com/users/user/subscriptions",
            "organizations_url": "https://api.github.com/users/user/orgs",
            "repos_url": "https://api.github.com/users/user/repos",
            "events_url": "https://api.github.com/users/user/events{/privacy}",
            "received_events_url": "https://api.github.com/users/user/received_events",
            "type": "User",
            "site_admin": false
        })
    }

    fn make_release(
        tag: &str,
        draft: bool,
        prerelease: bool,
        asset_names: &[&str],
    ) -> octocrab::models::repos::Release {
        make_release_with_digests(
            tag,
            draft,
            prerelease,
            &asset_names.iter().map(|n| (*n, None)).collect::<Vec<_>>(),
        )
    }

    /// `make_release`, with each asset's optional GitHub-declared digest.
    fn make_release_with_digests(
        tag: &str,
        draft: bool,
        prerelease: bool,
        asset_names: &[(&str, Option<&str>)],
    ) -> octocrab::models::repos::Release {
        let author = author_json();

        let assets: Vec<serde_json::Value> = asset_names
            .iter()
            .map(|(name, digest)| {
                serde_json::json!({
                    "url": "https://api.github.com/repos/test/test/releases/assets/1",
                    "id": 1,
                    "node_id": "MDEyO",
                    "name": name,
                    "label": null,
                    "content_type": "application/gzip",
                    "state": "uploaded",
                    "size": 1024,
                    "download_count": 0,
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z",
                    "browser_download_url": format!("https://github.com/test/test/releases/download/{tag}/{name}"),
                    "digest": digest,
                    "uploader": author,
                })
            })
            .collect();

        serde_json::from_value(serde_json::json!({
            "url": "https://api.github.com/repos/test/test/releases/1",
            "html_url": "https://github.com/test/test/releases/tag/v1.0.0",
            "assets_url": "https://api.github.com/repos/test/test/releases/1/assets",
            "upload_url": "https://uploads.github.com/repos/test/test/releases/1/assets{?name,label}",
            "id": 1,
            "node_id": "MDc6",
            "tag_name": tag,
            "target_commitish": "main",
            "name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "created_at": "2026-01-01T00:00:00Z",
            "published_at": "2026-01-01T00:00:00Z",
            "author": author,
            "assets": assets,
        }))
        .unwrap()
    }

    #[test]
    fn parse_standard_version_tag() {
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");
        let release = make_release("v3.28.0", false, false, &["cmake-3.28.0-linux-x86_64.tar.gz"]);

        let info = parse_release(&pattern, &release).unwrap();
        assert_eq!(info.version, "3.28.0");
        assert!(!info.is_prerelease);
        assert_eq!(info.assets.len(), 1);
        assert!(info.assets.contains_key("cmake-3.28.0-linux-x86_64.tar.gz"));
    }

    #[test]
    fn parse_prerelease_tag() {
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)(?:-(?P<prerelease>[0-9a-zA-Z]+))?$");
        let release = make_release("v3.28.0-rc1", false, true, &[]);

        let info = parse_release(&pattern, &release).unwrap();
        assert_eq!(info.version, "3.28.0-rc1");
        assert!(info.is_prerelease);
    }

    #[test]
    fn skip_draft_release() {
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");
        let release = make_release("v1.0.0", true, false, &[]);

        assert!(parse_release(&pattern, &release).is_none());
    }

    #[test]
    fn skip_non_matching_tag() {
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");
        let release = make_release("nightly-2026-01-01", false, false, &[]);

        assert!(parse_release(&pattern, &release).is_none());
    }

    /// A one-shot HTTP server answering each connection with the next canned
    /// status in `statuses`, counting what it served and recording the raw
    /// request text of each.
    async fn serve(statuses: Vec<u16>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>, Requests) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        let requests: Requests = Default::default();
        let recorder = requests.clone();
        tokio::spawn(async move {
            for status in statuses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let read = socket.read(&mut buf).await.unwrap_or(0);
                recorder
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..read]).into_owned());
                let body = if status == 200 { "[]" } else { r#"{"message":"boom"}"# };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        (base, served, requests)
    }

    /// The raw request text [`serve`] recorded, one entry per connection.
    type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    fn fixture_client(base: &str) -> octocrab::Octocrab {
        super::client(base, None).unwrap()
    }

    #[tokio::test]
    async fn a_5xx_page_is_retried_until_it_serves() {
        let (base, served, _requests) = serve(vec![502, 503, 200]).await;
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");

        let versions = list_versions(&fixture_client(&base), "o", "r", &pattern, 0)
            .await
            .unwrap();

        assert!(versions.is_empty());
        assert_eq!(
            served.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "two faults, one success"
        );
    }

    #[tokio::test]
    async fn a_4xx_page_fails_at_once_with_status_and_message() {
        let (base, served, _requests) = serve(vec![404, 200]).await;
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");

        let error = list_versions(&fixture_client(&base), "o", "r", &pattern, 0)
            .await
            .unwrap_err();

        assert_eq!(format!("{error:#}"), "GitHub API 404 Not Found: boom");
        assert_eq!(
            served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a 404 is not retried"
        );
    }

    /// The two headers the transport owns, because octocrab's service path
    /// carries no config layer to set them: GitHub refuses a request without
    /// a `User-Agent`, and the token is the difference between the 60/hour
    /// unauthenticated quota and 5 000/hour.
    #[tokio::test]
    async fn the_transport_sets_the_user_agent_and_the_token() {
        let (base, _served, requests) = serve(vec![200]).await;
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");

        let client = super::client(&base, Some("ghp_example")).unwrap();
        list_versions(&client, "o", "r", &pattern, 0).await.unwrap();

        let request = requests.lock().unwrap().first().cloned().expect("one request served");
        assert!(
            request.contains(&format!("user-agent: {USER_AGENT}")),
            "the listing request must carry the mirror's user agent: {request}"
        );
        assert!(
            request.contains("authorization: Bearer ghp_example"),
            "a supplied GITHUB_TOKEN must reach the wire: {request}"
        );
        assert!(
            request.starts_with("GET /repos/o/r/releases?"),
            "octocrab's relative route must be resolved against the configured base: {request}"
        );
    }

    #[tokio::test]
    async fn an_api_root_without_a_scheme_or_host_is_refused_up_front() {
        // Composing onto a half-built base yields a relative URI that reqwest
        // refuses at send time, which the retry ladder then treats as a
        // transient fault: five attempts and ~31s before anyone sees the
        // actual mistake.
        for bad in ["api.github.com", "/repos", "https://"] {
            let error = super::client(bad, None).expect_err("'{bad}' is not an API root");
            assert!(
                error.to_string().contains("GitHub API root"),
                "unexpected error for {bad:?}: {error}"
            );
        }
        super::client("https://api.github.com", None).expect("a full root builds");
    }

    /// Without a token the header is absent rather than empty — an empty
    /// `Authorization` is a 401, not an unauthenticated request.
    #[tokio::test]
    async fn no_token_sends_no_authorization_header() {
        let (base, _served, requests) = serve(vec![200]).await;
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");

        list_versions(&fixture_client(&base), "o", "r", &pattern, 0)
            .await
            .unwrap();

        let request = requests.lock().unwrap().first().cloned().expect("one request served");
        assert!(
            !request.to_ascii_lowercase().contains("authorization:"),
            "an unauthenticated listing must send no Authorization header: {request}"
        );
    }

    #[test]
    fn release_asset_digest_is_carried() {
        // `verify.github_asset_digest` verified nothing before #76 — the field
        // the policy reads is this one, so its capture is the whole feature.
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let release = make_release_with_digests(
            "v1.0.0",
            false,
            false,
            &[
                ("tool-linux-amd64.tar.gz", Some(&format!("sha256:{hex}"))),
                // An older release: GitHub omits the key entirely.
                ("tool-darwin-arm64.tar.gz", None),
            ],
        );

        let info = parse_release(&pattern, &release).unwrap();
        assert_eq!(info.assets.len(), 2);
        assert_eq!(info.asset_digests["tool-linux-amd64.tar.gz"], format!("sha256:{hex}"));
        assert!(
            !info.asset_digests.contains_key("tool-darwin-arm64.tar.gz"),
            "an asset with no declared digest must be absent, not empty"
        );
    }

    #[test]
    fn multiple_assets_collected() {
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");
        let release = make_release(
            "v1.0.0",
            false,
            false,
            &[
                "tool-1.0.0-linux-amd64.tar.gz",
                "tool-1.0.0-darwin-arm64.tar.gz",
                "tool-1.0.0-windows-amd64.zip",
            ],
        );

        let info = parse_release(&pattern, &release).unwrap();
        assert_eq!(info.assets.len(), 3);
    }
}
