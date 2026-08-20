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
    /// [`Publish::layout`], plus `dist.json` and `dist/<sha256>.json` at fixed
    /// paths.
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

    /// Whether a mirrored archive stays under [`Self::output`] after it has
    /// been uploaded.
    ///
    /// Three states. Unset is **auto** and is what almost every spec should
    /// use: retain when [`Self::upload`] is absent, discard when it is
    /// configured. The two modes want opposite things and the spec already
    /// says which one it is in — without an uploader the tree *is* the
    /// deliverable and must be complete; with one the store is the deliverable
    /// and the tree is a staging area.
    ///
    /// Discarding matters more than it sounds. A full ocx mirror is ~1.9 GB of
    /// archives, and the CI runners this is built for routinely have a few GB
    /// spare — staging the whole set before uploading any of it is what fills
    /// a runner's disk. With this off each archive is removed as soon as its
    /// upload is confirmed, so peak usage is bounded by
    /// `concurrency.max_downloads × largest_archive` rather than by the size
    /// of the whole mirror.
    ///
    /// `true` forces retention even when uploading, for an operator who ships
    /// the tree *and* the store. Governs archives only: `dist.json` and
    /// `dist/<sha256>.json` are a few KB, are what the report names, and are
    /// always written.
    ///
    /// [`Self::retain_archives_resolved`] applies the auto rule.
    #[serde(default)]
    pub retain_archives: Option<bool>,

    /// Hosts allowed to be reached over plaintext `http://`.
    ///
    /// Same doctrine as `RegistrySpec`: the manifest is the control plane
    /// naming every version and digest the run trusts, so plaintext is refused
    /// by default. Entries are exact hosts or CIDR blocks; the acceptance
    /// harness lists its loopback address here.
    #[serde(default)]
    pub trusted_hosts: Vec<String>,

    /// How wide the archive download and upload passes run.
    #[serde(default)]
    pub concurrency: DistConcurrency,
}

/// How many archives this run moves at once.
///
/// Two knobs, not [`ConcurrencyConfig`](crate::spec::ConcurrencyConfig)'s five:
/// `dist sync` neither bundles nor compresses, so `max_bundles` and
/// `compression_threads` would govern nothing. Modelled on
/// [`RegistryConcurrency`](crate::spec::RegistryConcurrency) instead, which
/// made the same cut for the same reason.
///
/// Downloads and uploads are separate because they are separate resources: a
/// run pulls from GitHub's object host and pushes into a corporate store, and
/// the store is usually the one with a rate limit worth respecting.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DistConcurrency {
    /// Archives fetched at once. Peak resident memory is
    /// `max_downloads × largest_archive`, because each body is buffered whole
    /// before it is written and verified.
    #[serde(default = "default_max_downloads")]
    pub max_downloads: usize,

    /// Archives uploaded at once — effectively capped by [`Self::max_downloads`]
    /// too, because each upload runs inside its own row's pass. The rolling
    /// manifest and the snapshot are **never** included: their ordering is the
    /// publish invariant (`pipeline::dist_sync::upload_manifest`).
    #[serde(default = "default_max_uploads")]
    pub max_uploads: usize,
}

impl Default for DistConcurrency {
    fn default() -> Self {
        Self {
            max_downloads: default_max_downloads(),
            max_uploads: default_max_uploads(),
        }
    }
}

fn default_max_downloads() -> usize {
    8
}

/// Lower than the download default: the destination is one corporate store
/// answering every request, where the source is a CDN.
fn default_max_uploads() -> usize {
    4
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
    /// [`Self::retain_archives`] with the auto rule applied.
    ///
    /// Auto is derived from `upload:` rather than defaulting to a constant
    /// because the two modes genuinely want opposite answers, and the spec has
    /// already declared which mode it is in. A plain `#[serde(default)]` bool
    /// would have to pick one of them and be wrong for the other half of
    /// every fleet.
    #[must_use]
    pub fn retain_archives_resolved(&self) -> bool {
        self.retain_archives.unwrap_or(self.upload.is_none())
    }

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

        // Refused rather than clamped to 1: zero transfers nothing, and a run
        // that silently did nothing would still write a manifest.
        if self.concurrency.max_downloads == 0 {
            errors.push(
                "concurrency.max_downloads: must be at least 1 — zero archives in flight mirrors nothing".to_string(),
            );
        }
        if self.concurrency.max_uploads == 0 {
            errors.push(
                "concurrency.max_uploads: must be at least 1 — zero files in flight publishes nothing".to_string(),
            );
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
