// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `dist.yml` — the OCX distribution mirror spec (`ocx-mirror dist sync`).
//!
//! A third root type beside [`MirrorSpec`](crate::spec::MirrorSpec) and
//! [`RegistrySpec`](crate::spec::RegistrySpec). Where those two mirror *OCX
//! packages*, this one mirrors the **bootstrap layer**: the ocx release
//! archives plus `dist.json`, the manifest `install.sh`, `rules_ocx`,
//! `find_ocx` and the SDKs resolve versions and checksums from. Nothing here
//! touches an OCI registry.
//!
//! The `kind:` discriminator that tells the three apart is read only by the
//! pre-scan ([`crate::spec::pre_scan`]), never by this type.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use ocx_lib::oci::ssrf::host_is_trusted;
use serde::Deserialize;
use url::Url;

/// The upstream manifest every mirror run starts from.
const DEFAULT_SOURCE: &str = "https://setup.ocx.sh/dist.json";

/// The layout every store that serves plain paths wants, and the one the
/// installers' own `${OCX_INSTALL_MIRROR_URL}/${tag}/${filename}` rewrite
/// produces.
const DEFAULT_LAYOUT: &str = "{tag}/{filename}";

/// Backoff schedule when `upload.retry_delays` is omitted, in seconds.
///
/// The array *is* the retry count — there is deliberately no separate
/// `max_retries` field that could contradict it, and `[]` disables retry.
const DEFAULT_RETRY_DELAYS: &[u64] = &[1, 5, 10, 30, 60];

/// The `dist.yml` root document.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DistSpec {
    /// The upstream `dist.json` to mirror. Defaults to
    /// `https://setup.ocx.sh/dist.json`.
    ///
    /// Fetched, never re-derived from the GitHub Releases API: target
    /// extraction, channel semantics and the `latest` pointers live in
    /// `www-setup/scripts/gen-dist.sh`, and a second implementation here would
    /// drift from it silently.
    // `with = "String"`, not schemars' own `url2` feature: `schemars` is
    // shared with `ocx_lib`, and its feature list is copied verbatim from
    // ocx's `[workspace.dependencies]` (see CLAUDE.md). Adding a feature here
    // would diverge and be dropped by the next submodule re-sync — and a URL
    // is a JSON string either way.
    #[cfg_attr(feature = "jsonschema", schemars(with = "String"))]
    #[serde(default = "default_source")]
    pub source: Url,

    /// Directory the mirror tree is written into — archives at the rendered
    /// [`Publish::layout`], plus `dist.json`, `dist.json.sha256` and
    /// `dist/<sha256>.json` at fixed paths.
    ///
    /// Always written, whether or not [`Self::upload`] is configured: the
    /// operator's own `aws s3 sync` / `rsync` / commit step is the path that
    /// works against every store.
    pub output: PathBuf,

    /// Which upstream releases to keep. Every filter is subtractive and they
    /// combine with AND — a release survives iff every present filter accepts
    /// it, so a future filter composes without a precedence rule.
    #[serde(default)]
    pub select: Select,

    /// Where the mirrored archives will be served from, and under what path
    /// shape. Drives both the emitted tree and the `url` written into every
    /// mirrored manifest row.
    pub publish: Publish,

    /// Optional native HTTP PUT of the emitted tree. Omit to emit only.
    pub upload: Option<Upload>,

    /// Hosts allowed to be reached over plaintext `http://`.
    ///
    /// Same doctrine as `RegistrySpec`: the manifest is the control plane
    /// naming every version and digest the run trusts, so plaintext is refused
    /// by default. Entries are exact hosts or CIDR blocks; the acceptance
    /// harness lists its loopback address here.
    #[serde(default)]
    pub trusted_hosts: Vec<String>,
}

fn default_source() -> Url {
    Url::parse(DEFAULT_SOURCE).expect("the compiled-in default source is a valid URL")
}

/// Which upstream releases survive into the mirror.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Select {
    /// Inclusive lower version bound.
    ///
    /// Semver-ordered, so `min_version: "1.0.0"` **excludes** `1.0.0-rc.1` —
    /// a prerelease sorts below its own release.
    pub min_version: Option<String>,
}

/// Where the mirrored bytes will be served from.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Publish {
    /// The public base every mirrored `url` is composed from. Trailing
    /// slashes are ignored.
    #[cfg_attr(feature = "jsonschema", schemars(with = "String"))]
    pub base_url: Url,

    /// Path shape below [`Self::base_url`], as plain substitution over
    /// `{version}`, `{tag}`, `{target}`, `{filename}` and `{channel}`.
    ///
    /// Defaults to `{tag}/{filename}` — the layout a plain file store wants
    /// and the one the installers' own mirror rewrite already produces. A
    /// GitLab generic package registry needs `{version}/{filename}`, which is
    /// the whole reason this is configurable and the reason the mirrored
    /// manifest rewrites `url` rather than leaving consumers to compose it.
    #[serde(default = "default_layout")]
    pub layout: String,
}

fn default_layout() -> String {
    DEFAULT_LAYOUT.to_string()
}

/// Native HTTP PUT of the emitted tree.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Upload {
    /// Credentials, resolved from the environment at run time. Omit for a
    /// store that accepts anonymous writes.
    pub identity: Option<Identity>,

    /// Backoff schedule in seconds. The array length is the retry count;
    /// `[]` disables retry entirely.
    #[serde(default = "default_retry_delays")]
    pub retry_delays: Vec<u64>,

    /// Extra request headers, sent verbatim on every PUT.
    ///
    /// The escape hatch that keeps one PUT implementation covering stores with
    /// per-vendor quirks — Azure Blob's `x-ms-blob-type: BlockBlob`, GitLab's
    /// `JOB-TOKEN`. `Authorization` is refused here; it belongs in
    /// [`Self::identity`], which reads its value from the environment.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_retry_delays() -> Vec<u64> {
    DEFAULT_RETRY_DELAYS.to_vec()
}

/// How the uploader authenticates.
///
/// Internally tagged, so `token_env` under `type: basic` is a load error
/// rather than a silently ignored key — the invalid combinations are
/// unrepresentable instead of being a validation rule someone forgets.
///
/// **Every field is an environment variable name, never a value.** There is no
/// literal variant, so a credential cannot reach a committed spec even by
/// accident. The field is spelled `identity:` rather than `auth:` because
/// [`CREDENTIAL_DENY_LIST`](crate::spec::CREDENTIAL_DENY_LIST) refuses an
/// `auth` key at any depth, and weakening that guard to admit a block that
/// holds no secrets would weaken it for the blocks that do.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Identity {
    /// `Authorization: Bearer <$token_env>`.
    Bearer {
        /// Environment variable holding the bearer token.
        token_env: String,
    },
    /// HTTP Basic. Both halves come from the environment — a username is
    /// routinely inherited from an outer CI project rather than written down
    /// per repository.
    Basic {
        /// Environment variable holding the user name.
        username_env: String,
        /// Environment variable holding the password.
        password_env: String,
    },
}

impl DistSpec {
    /// Every validation rule, one message per violation.
    ///
    /// Returns an empty vector for a valid spec; the caller maps a non-empty
    /// one to [`MirrorError::SpecInvalid`](crate::error::MirrorError::SpecInvalid)
    /// (exit 65). Reports every violation rather than the first, so an
    /// operator fixes one spec once.
    ///
    /// `spec_path` is accepted so this reads exactly like the other two root
    /// types at every call site. It is deliberately unread: no rule below is
    /// path-relative.
    pub fn validate(&self, _spec_path: &Path) -> Vec<String> {
        let mut errors = Vec::new();

        if self.output.as_os_str().is_empty() {
            errors.push("output: a directory to write the mirror tree into is required".to_string());
        }

        self.validate_transport("source", &self.source, &mut errors);
        self.validate_transport("publish.base_url", &self.publish.base_url, &mut errors);
        self.validate_publish_base(&mut errors);

        if let Err(error) = crate::pipeline::dist_sync::layout::LayoutTemplate::parse(&self.publish.layout) {
            errors.push(format!("publish.layout: {error}"));
        }

        if let Some(upload) = &self.upload {
            for name in upload.headers.keys() {
                if name.eq_ignore_ascii_case("authorization") {
                    errors.push(
                        "upload.headers: 'Authorization' must not be set here — use `identity:`, whose \
                         values come from the environment"
                            .to_string(),
                    );
                }
            }
        }

        errors
    }

    /// Refuse a plaintext URL whose host is not explicitly trusted, and any
    /// URL that embeds userinfo.
    ///
    /// Userinfo is refused rather than stripped: `https://user:pass@host/` in
    /// `source:` is a credential in a committed file, and in
    /// `publish.base_url` it would be copied into every mirrored manifest row
    /// and served to every consumer.
    fn validate_transport(&self, field: &str, url: &Url, errors: &mut Vec<String>) {
        if !url.username().is_empty() || url.password().is_some() {
            errors.push(format!(
                "{field}: the URL must not embed credentials; put them in the environment instead"
            ));
        }

        match url.scheme() {
            "https" => {}
            "http" => {
                let host = url.host_str().unwrap_or_default().to_string();
                if !host_is_trusted(&host, &self.trusted_hosts) {
                    errors.push(format!(
                        "{field}: '{}' is a plaintext transport, and this manifest is the control plane \
                         naming every version and digest the run trusts; use https, or add '{host}' to \
                         `trusted_hosts:`",
                        url.scheme()
                    ));
                }
            }
            scheme => errors.push(format!("{field}: '{scheme}' is not a supported scheme; use https")),
        }
    }

    /// Refuse a `publish.base_url` carrying a query or a fragment.
    ///
    /// Two consumers compose onto this base and they cannot agree about one:
    /// [`mirrored_url`](crate::pipeline::dist_sync::mirrored_url) concatenates
    /// the URL as written, so a query survives into every published row, while
    /// the uploader composes through `path_segments_mut`, which drops it. The
    /// same byte would then be advertised at one URL and stored at another.
    ///
    /// The reachable case is the one the documentation invites: an Azure Blob
    /// SAS is a query string, so a base carrying one would copy a live
    /// write credential into a manifest served to every consumer — while the
    /// upload itself still succeeded, leaving nothing to notice.
    ///
    /// Refused rather than stripped, matching this module's doctrine
    /// everywhere else: an operator who put a query there meant something by
    /// it, and silently dropping it would upload to a URL they did not write.
    fn validate_publish_base(&self, errors: &mut Vec<String>) {
        let base = &self.publish.base_url;
        if base.query().is_some() || base.fragment().is_some() {
            errors.push(
                "publish.base_url: must carry no query or fragment — it is composed onto for both the \
                 published URL and the upload target, and the two compose it differently; put a SAS or \
                 signed-URL credential in `upload.identity` or `upload.headers` instead"
                    .to_string(),
            );
        }
    }
}

#[cfg(test)]
#[path = "dist/tests.rs"]
mod tests;
