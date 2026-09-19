// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

use super::VersionInfo;
use crate::spec::GeneratorConfig;

/// Root of the url_index JSON format.
///
/// Contains a map of version strings to their release assets.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
pub struct RemoteIndex {
    /// Map of version string (e.g., "22.15.0") to version entry.
    pub versions: HashMap<String, RemoteVersionEntry>,
}

/// A single version's metadata and download assets.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
pub struct RemoteVersionEntry {
    /// Whether this is a pre-release version.
    #[serde(default)]
    pub prerelease: bool,
    /// Map of asset filename to its download URL, bare or digest-carrying.
    pub assets: HashMap<String, IndexAsset>,
}

/// One asset in a url_index document: a bare download URL, or an object
/// carrying the URL plus the publisher's sha256.
///
/// Untagged, so the schema renders the two forms as alternatives and every
/// generator written against v1 keeps validating — additive, so the document
/// stays url-index **v1** under the same `$id`.
///
/// ```json
/// "assets": {
///   "tool-linux-amd64.tar.gz": "https://example.com/tool-linux-amd64.tar.gz",
///   "tool-darwin-arm64.tar.gz": { "url": "https://…", "sha256": "9f86d081…" }
/// }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum IndexAsset {
    /// `"<name>": "<url>"`
    Url(String),
    /// `"<name>": { "url": "<url>", "sha256": "<hex>" }`
    ///
    /// `sha256` is required: an object without it is a verbose string.
    Verified { url: String, sha256: String },
}

impl IndexAsset {
    /// The download URL, whichever form declared it.
    pub fn url(&self) -> &str {
        match self {
            Self::Url(url) => url,
            Self::Verified { url, .. } => url,
        }
    }

    /// The digest exactly as the document spelled it, before normalisation.
    pub fn digest(&self) -> Option<&str> {
        match self {
            Self::Url(_) => None,
            Self::Verified { sha256, .. } => Some(sha256),
        }
    }
}

/// A declared digest as `verify_digest` will re-parse it at download time.
///
/// A bare hex string is the common spelling in a hand-written index, so it is
/// promoted to `sha256:<hex>`; an already-prefixed value passes through, which
/// is what keeps a stronger algorithm expressible without a schema change.
/// `ocx_oci`'s own parser is the check — the grammar has to be the one the
/// download leg applies, not a second hex test that agrees with it by luck.
fn normalized_digest(raw: &str) -> anyhow::Result<String> {
    let candidate = match raw.contains(':') {
        true => raw.to_string(),
        false => format!("sha256:{raw}"),
    };
    ocx_oci::Digest::try_from(candidate.as_str())?;
    Ok(candidate)
}

/// Split one version's asset map into its URLs and its declared digests.
///
/// Both are validated here, at crawl time: malformed foreign input fails
/// before a gigabyte moves, and never becomes control flow downstream.
fn split_assets(
    assets: &HashMap<String, IndexAsset>,
    version: &str,
) -> anyhow::Result<(HashMap<String, Url>, HashMap<String, String>)> {
    let mut urls = HashMap::with_capacity(assets.len());
    let mut digests = HashMap::new();
    for (name, asset) in assets {
        let url = Url::parse(asset.url())
            .map_err(|e| anyhow::anyhow!("invalid URL for asset '{name}' in version '{version}': {e}"))?;
        if let Some(raw) = asset.digest() {
            let digest = normalized_digest(raw)
                .map_err(|e| anyhow::anyhow!("invalid sha256 for asset '{name}' in version '{version}': {e}"))?;
            digests.insert(name.clone(), digest);
        }
        urls.insert(name.clone(), url);
    }
    Ok((urls, digests))
}

/// Parse a `RemoteIndex` into a list of `VersionInfo` entries.
fn parse_remote_index(index: RemoteIndex) -> anyhow::Result<Vec<VersionInfo>> {
    let mut versions = Vec::with_capacity(index.versions.len());
    for (version, entry) in &index.versions {
        let (assets, asset_digests) = split_assets(&entry.assets, version)?;

        versions.push(VersionInfo {
            version: version.clone(),
            assets,
            asset_digests,
            is_prerelease: entry.prerelease,
        });
    }
    Ok(versions)
}

/// Convert inline versions from the mirror spec into `VersionInfo` entries.
pub fn from_inline(versions: &HashMap<String, crate::spec::UrlIndexVersion>) -> anyhow::Result<Vec<VersionInfo>> {
    let mut result = Vec::with_capacity(versions.len());
    for (version, entry) in versions {
        let (assets, asset_digests) = split_assets(&entry.assets, version)?;

        result.push(VersionInfo {
            version: version.clone(),
            assets,
            asset_digests,
            is_prerelease: entry.prerelease,
        });
    }
    Ok(result)
}

/// Fetch versions from a remote JSON URL. The JSON format matches the inline `versions` schema:
/// `{ "versions": { "<ver>": { "prerelease": bool, "assets": { "<name>": "<url>" } } } }`
pub async fn from_remote(url: &str) -> anyhow::Result<Vec<VersionInfo>> {
    // Through the factory, not `reqwest::get`: the latter builds a bare
    // client per call, which is a leg the operator's extra CA never reaches.
    let response = crate::http::client()?.get(url).send().await?.error_for_status()?;
    let index: RemoteIndex = response.json().await?;
    parse_remote_index(index)
}

/// Run a generator command and parse its stdout as url_index JSON.
pub async fn from_generator(config: &GeneratorConfig, spec_dir: &Path) -> anyhow::Result<Vec<VersionInfo>> {
    let stdout = super::generator::run(config, spec_dir).await?;
    let index: RemoteIndex = serde_json::from_slice(&stdout)
        .map_err(|e| anyhow::anyhow!("generator output is not valid url_index JSON: {e}"))?;
    parse_remote_index(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::UrlIndexVersion;

    #[test]
    fn inline_versions() {
        let mut versions = HashMap::new();
        let mut assets = HashMap::new();
        assets.insert(
            "tool-1.0.0-linux-amd64.tar.gz".to_string(),
            IndexAsset::Url("https://example.com/tool-1.0.0-linux-amd64.tar.gz".to_string()),
        );
        versions.insert(
            "1.0.0".to_string(),
            UrlIndexVersion {
                prerelease: false,
                assets,
            },
        );

        let result = from_inline(&versions).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "1.0.0");
        assert!(!result[0].is_prerelease);
        assert_eq!(result[0].assets.len(), 1);
    }

    #[test]
    fn inline_prerelease_flag() {
        let mut versions = HashMap::new();
        versions.insert(
            "2.0.0-rc1".to_string(),
            UrlIndexVersion {
                prerelease: true,
                assets: HashMap::new(),
            },
        );

        let result = from_inline(&versions).unwrap();
        assert!(result[0].is_prerelease);
    }

    #[test]
    fn inline_empty() {
        let versions = HashMap::new();
        let result = from_inline(&versions).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_remote_index_basic() {
        let mut versions = HashMap::new();
        versions.insert(
            "1.0.0".to_string(),
            RemoteVersionEntry {
                prerelease: false,
                assets: HashMap::from([(
                    "tool-linux.tar.gz".to_string(),
                    IndexAsset::Url("https://example.com/tool-linux.tar.gz".to_string()),
                )]),
            },
        );
        versions.insert(
            "2.0.0-rc1".to_string(),
            RemoteVersionEntry {
                prerelease: true,
                assets: HashMap::from([(
                    "tool-linux.tar.gz".to_string(),
                    IndexAsset::Url("https://example.com/tool-2-linux.tar.gz".to_string()),
                )]),
            },
        );

        let index = RemoteIndex { versions };
        let result = parse_remote_index(index).unwrap();
        assert_eq!(result.len(), 2);

        let v1 = result.iter().find(|v| v.version == "1.0.0").unwrap();
        assert!(!v1.is_prerelease);
        assert_eq!(v1.assets.len(), 1);

        let v2 = result.iter().find(|v| v.version == "2.0.0-rc1").unwrap();
        assert!(v2.is_prerelease);
    }

    #[tokio::test]
    async fn from_generator_valid_output() {
        let config = GeneratorConfig {
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                r#"echo '{"versions":{"1.0.0":{"prerelease":false,"assets":{"tool.tar.gz":"https://example.com/tool.tar.gz"}}}}'"#.to_string(),
            ],
            working_directory: None,
            timeout_seconds: 10,
        };

        let result = from_generator(&config, Path::new(".")).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].version, "1.0.0");
        assert!(!result[0].is_prerelease);
        assert_eq!(result[0].assets.len(), 1);
    }

    #[tokio::test]
    async fn from_generator_nonzero_exit() {
        let config = GeneratorConfig {
            command: vec!["sh".to_string(), "-c".to_string(), "echo err >&2; exit 1".to_string()],
            working_directory: None,
            timeout_seconds: 10,
        };

        let err = from_generator(&config, Path::new(".")).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed"), "Expected failure message, got: {msg}");
        assert!(msg.contains("err"), "Expected stderr in message, got: {msg}");
    }

    #[tokio::test]
    async fn from_generator_empty_output() {
        let config = GeneratorConfig {
            command: vec!["true".to_string()],
            working_directory: None,
            timeout_seconds: 10,
        };

        let err = from_generator(&config, Path::new(".")).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no output"), "Expected 'no output' error, got: {msg}");
    }

    #[tokio::test]
    async fn from_generator_invalid_json() {
        let config = GeneratorConfig {
            command: vec!["echo".to_string(), "not json".to_string()],
            working_directory: None,
            timeout_seconds: 10,
        };

        let err = from_generator(&config, Path::new(".")).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not valid"), "Expected JSON parse error, got: {msg}");
    }

    #[tokio::test]
    async fn from_generator_timeout() {
        let config = GeneratorConfig {
            command: vec!["sleep".to_string(), "10".to_string()],
            working_directory: None,
            timeout_seconds: 1,
        };

        let err = from_generator(&config, Path::new(".")).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "Expected timeout error, got: {msg}");
    }

    #[tokio::test]
    async fn from_generator_command_not_found() {
        let config = GeneratorConfig {
            command: vec!["nonexistent-command-xyz-12345".to_string()],
            working_directory: None,
            timeout_seconds: 10,
        };

        let err = from_generator(&config, Path::new(".")).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to run"), "Expected spawn failure, got: {msg}");
    }

    /// A url_index document with one asset per named form.
    fn index_json(asset: &str) -> RemoteIndex {
        serde_json::from_str(&format!(
            r#"{{"versions":{{"1.0.0":{{"assets":{{"tool.tar.gz":{asset}}}}}}}}}"#
        ))
        .expect("document must parse")
    }

    const HEX: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[test]
    fn object_asset_carries_digest() {
        let index = index_json(&format!(
            r#"{{"url":"https://example.com/tool.tar.gz","sha256":"{HEX}"}}"#
        ));
        let result = parse_remote_index(index).unwrap();

        assert_eq!(
            result[0].assets["tool.tar.gz"].as_str(),
            "https://example.com/tool.tar.gz"
        );
        assert_eq!(result[0].asset_digests["tool.tar.gz"], format!("sha256:{HEX}"));
    }

    #[test]
    fn bare_hex_digest_is_normalised_to_sha256_prefix() {
        // The hand-written spelling: `sha256sum` output pasted verbatim. It
        // has to reach `verify_digest` as an OCI digest or the download leg
        // rejects the operator's own correct value.
        let index = index_json(&format!(
            r#"{{"url":"https://example.com/tool.tar.gz","sha256":"{HEX}"}}"#
        ));
        let result = parse_remote_index(index).unwrap();
        assert_eq!(result[0].asset_digests["tool.tar.gz"], format!("sha256:{HEX}"));
    }

    #[test]
    fn prefixed_digest_passes_through() {
        let index = index_json(&format!(
            r#"{{"url":"https://example.com/tool.tar.gz","sha256":"sha256:{HEX}"}}"#
        ));
        let result = parse_remote_index(index).unwrap();
        assert_eq!(result[0].asset_digests["tool.tar.gz"], format!("sha256:{HEX}"));
    }

    #[test]
    fn malformed_digest_is_refused() {
        // Fail closed at crawl: a truncated hex that reached the download leg
        // would fail there too, but only after the bytes moved, and per
        // platform rather than once.
        let index = index_json(r#"{"url":"https://example.com/tool.tar.gz","sha256":"deadbeef"}"#);
        let err = parse_remote_index(index).unwrap_err().to_string();
        assert!(err.contains("invalid sha256 for asset 'tool.tar.gz'"), "{err}");
        assert!(err.contains("in version '1.0.0'"), "{err}");
    }

    #[test]
    fn string_and_object_assets_coexist_in_one_version() {
        // The untagged enum's regression guard: a v1 document that gained one
        // object asset must keep the rest of its bare strings working.
        let index: RemoteIndex = serde_json::from_str(&format!(
            r#"{{"versions":{{"1.0.0":{{"assets":{{
                 "bare.tar.gz":"https://example.com/bare.tar.gz",
                 "checked.tar.gz":{{"url":"https://example.com/checked.tar.gz","sha256":"{HEX}"}}
               }}}}}}}}"#
        ))
        .unwrap();

        let result = parse_remote_index(index).unwrap();
        assert_eq!(result[0].assets.len(), 2);
        assert_eq!(result[0].asset_digests.len(), 1, "only the object form declares one");
        assert!(result[0].asset_digests.contains_key("checked.tar.gz"));
    }

    #[test]
    fn inline_object_asset_carries_digest() {
        // The same two forms reaching `VersionInfo` through the spec's inline
        // twin rather than a fetched document.
        let versions = HashMap::from([(
            "1.0.0".to_string(),
            UrlIndexVersion {
                prerelease: false,
                assets: HashMap::from([(
                    "tool.tar.gz".to_string(),
                    IndexAsset::Verified {
                        url: "https://example.com/tool.tar.gz".to_string(),
                        sha256: HEX.to_string(),
                    },
                )]),
            },
        )]);

        let result = from_inline(&versions).unwrap();
        assert_eq!(result[0].asset_digests["tool.tar.gz"], format!("sha256:{HEX}"));
    }

    #[test]
    fn parse_remote_index_invalid_url() {
        let mut versions = HashMap::new();
        versions.insert(
            "1.0.0".to_string(),
            RemoteVersionEntry {
                prerelease: false,
                assets: HashMap::from([("tool.tar.gz".to_string(), IndexAsset::Url("not-a-url".to_string()))]),
            },
        );

        let index = RemoteIndex { versions };
        let result = parse_remote_index(index);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid URL"), "Expected URL error, got: {err}");
    }
}
