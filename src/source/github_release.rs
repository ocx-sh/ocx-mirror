// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;
use std::time::Duration;

use regex::Regex;

use super::VersionInfo;
use crate::pipeline::ocx_cli::push::jitter;

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

/// The builder every GitHub listing client starts from.
///
/// octocrab's own retry layer is switched off so [`list_versions`]'s ladder is
/// the only one: the default `Simple(3)` fires three retries back-to-back with
/// no delay, which any blip longer than a round-trip defeats, and stacking the
/// two would multiply requests during an outage.
pub(crate) fn builder() -> octocrab::OctocrabBuilder<
    octocrab::NoSvc,
    octocrab::DefaultOctocrabBuilderConfig,
    octocrab::NoAuth,
    octocrab::NotLayerReady,
> {
    octocrab::Octocrab::builder().add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
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
    for asset in &release.assets {
        assets.insert(asset.name.clone(), asset.browser_download_url.clone());
    }

    Some(VersionInfo {
        version: full_version,
        assets,
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
        let author = author_json();

        let assets: Vec<serde_json::Value> = asset_names
            .iter()
            .map(|name| {
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
    /// status in `statuses`, counting what it served.
    async fn serve(statuses: Vec<u16>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        tokio::spawn(async move {
            for status in statuses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let body = if status == 200 { "[]" } else { r#"{"message":"boom"}"# };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        (base, served)
    }

    fn client(base: &str) -> octocrab::Octocrab {
        builder().base_uri(base).unwrap().build().unwrap()
    }

    #[tokio::test]
    async fn a_5xx_page_is_retried_until_it_serves() {
        let (base, served) = serve(vec![502, 503, 200]).await;
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");

        let versions = list_versions(&client(&base), "o", "r", &pattern, 0).await.unwrap();

        assert!(versions.is_empty());
        assert_eq!(
            served.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "two faults, one success"
        );
    }

    #[tokio::test]
    async fn a_4xx_page_fails_at_once_with_status_and_message() {
        let (base, served) = serve(vec![404, 200]).await;
        let pattern = make_pattern(r"^v(?P<version>\d+\.\d+\.\d+)$");

        let error = list_versions(&client(&base), "o", "r", &pattern, 0).await.unwrap_err();

        assert_eq!(format!("{error:#}"), "GitHub API 404 Not Found: boom");
        assert_eq!(
            served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a 404 is not retried"
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
