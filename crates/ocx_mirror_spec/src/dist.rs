// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `dist.yml` — the OCX distribution mirror spec (`ocx-mirror dist sync`).
//!
//! A third root type beside [`MirrorSpec`](crate::MirrorSpec) and
//! [`RegistrySpec`](crate::RegistrySpec). Where those two mirror *OCX
//! packages*, this one mirrors the **bootstrap layer**: the ocx release
//! archives plus `dist.json`, the manifest `install.sh`, `rules_ocx`,
//! `find_ocx` and the SDKs resolve versions and checksums from. Nothing here
//! touches an OCI registry.
//!
//! The `kind:` discriminator that tells the three apart is read only by the
//! pre-scan ([`crate::pre_scan`]), never by this type.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use ocx_oci::ssrf::host_is_trusted;
use serde::Deserialize;
use url::Url;

use crate::ValueSource;
use crate::layout::{InstallerTemplate, InstallerTemplateKind, InstallerValues};

/// The upstream manifest every mirror run starts from.
const DEFAULT_SOURCE: &str = "https://setup.ocx.sh/dist.json";

/// The layout every store that serves plain paths wants, and the one the
/// installers' own `${OCX_INSTALL_MIRROR_URL}/${tag}/${filename}` rewrite
/// produces.
const DEFAULT_LAYOUT: &str = "{tag}/{filename}";

/// Where the rolling manifest lands when `publish.dist.path` is omitted.
const DEFAULT_DIST_PATH: &str = "dist.json";

/// Where a content-addressed snapshot lands when `publish.dist.snapshots` is
/// omitted.
const DEFAULT_SNAPSHOT_LAYOUT: &str = "dist/{sha256}.json";

/// Where a rolling installer lands when `publish.installers.path` is omitted:
/// the root, beside the rolling `dist.json`.
const DEFAULT_INSTALLER_PATH: &str = "{filename}";

/// Where a pinned installer lands when `publish.installers.snapshots` is
/// omitted.
const DEFAULT_INSTALLER_SNAPSHOTS: &str = "install/{sha256}/{filename}";

/// The upstream installers — setup.ocx.sh's *stored* stable pointer, not the
/// `/sh` friendly path, which only exists as a CDN rewrite onto it.
const DEFAULT_INSTALLER_SOURCE: &str = "https://setup.ocx.sh/latest/{shell}";

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
    // shared with ocx, and its feature list is copied verbatim from
    // ocx's `[workspace.dependencies]` (see CLAUDE.md). Adding a feature here
    // would diverge and be dropped by the next submodule re-sync — and a URL
    // is a JSON string either way.
    #[cfg_attr(feature = "jsonschema", schemars(with = "String"))]
    #[serde(default = "default_source")]
    pub source: Url,

    /// Directory the mirror tree is written into — archives at the rendered
    /// [`Publish::layout`], the manifest documents at [`Publish::dist`].
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
    /// the tree *and* the store. Governs archives only: the manifest documents
    /// are a few KB, are what the report names, and are always written.
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
    /// Archives fetched at once. Each body is buffered whole before it is
    /// written and verified, so downloads alone hold up to
    /// `max_downloads × largest_archive`. A row then uploads inside the same
    /// pass, and the PUT buffers the body again to checksum it, so an uploading
    /// row can be resident at the same time as a downloading one: peak memory is
    /// bounded by `(max_downloads + max_uploads) × largest_archive`, not by
    /// `max_downloads` alone.
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

    /// Where the two manifest documents land, and whether they are uploaded.
    ///
    /// `false` uploads archives only; the block sets the paths; omitted (or
    /// `true`) is the block with its defaults. Whatever is set, both documents
    /// are always written under `output:` — the tree is the deliverable that
    /// works against every store, and "publish my own `dist.json`" starts
    /// from the one written there.
    #[serde(default)]
    pub dist: DistPublish,

    /// Site-patched copies of the setup.ocx.sh installers, published beside
    /// the manifest. **Opt-in**: absent or `false` publishes none, so a mirror
    /// that predates this key emits exactly the tree it always did; `true` is
    /// the block with its defaults. See [`InstallersLayout`].
    #[serde(default)]
    pub installers: InstallersPublish,
}

fn default_layout() -> String {
    DEFAULT_LAYOUT.to_string()
}

/// `publish.dist` — `false`, `true`, or the block.
///
/// Untagged so the spec reads `dist: false` rather than a nested switch. A
/// bare string is deliberately not accepted as shorthand for `path:` — a
/// shorthand that reads as "the manifest goes here" while silently keeping
/// every other default would be a surprise either way.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum DistPublish {
    /// `true` is the default block; `false` uploads neither document.
    Switch(bool),
    Layout(DistLayout),
}

impl Default for DistPublish {
    fn default() -> Self {
        Self::Layout(DistLayout::default())
    }
}

/// Where each manifest document lands below `base_url` (and `output:`).
///
/// Two documents, two shapes: the rolling manifest is one file, so `path` is
/// a plain path; a snapshot is one file *per manifest digest*, so `snapshots`
/// is a template over `{sha256}`. The defaults reproduce the fixed tree
/// earlier releases wrote, so an existing consumer's `OCX_INSTALL_DIST_URL`
/// does not move.
///
/// A GitLab generic package registry, which addresses every file as
/// `<package>/<version>/<file>`, is the reason this is configurable at all:
/// `path: dist/latest.json` puts the rolling manifest into package version
/// `dist`, beside the snapshots the default already lands there.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DistLayout {
    /// Plain relative path of the rolling manifest. Defaults to `dist.json`.
    #[serde(default = "default_dist_path")]
    pub path: String,

    /// Template for the content-addressed snapshot, over `{sha256}`, or
    /// `false` to upload none. Defaults to `dist/{sha256}.json`.
    #[serde(default)]
    pub snapshots: Snapshots,
}

impl Default for DistLayout {
    fn default() -> Self {
        Self {
            path: default_dist_path(),
            snapshots: Snapshots::default(),
        }
    }
}

fn default_dist_path() -> String {
    DEFAULT_DIST_PATH.to_string()
}

/// `publish.dist.snapshots` — `false`, `true`, or a template.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Snapshots {
    /// `true` is the default template; `false` uploads no snapshot.
    Switch(bool),
    Template(String),
}

impl Default for Snapshots {
    fn default() -> Self {
        Self::Template(DEFAULT_SNAPSHOT_LAYOUT.to_string())
    }
}

/// `publish.installers` — `false` (the default), `true`, or the block.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum InstallersPublish {
    /// `true` is the default block; `false` publishes no installer.
    Switch(bool),
    Layout(Box<InstallersLayout>),
}

impl Default for InstallersPublish {
    fn default() -> Self {
        Self::Switch(false)
    }
}

/// Where the patched installers land, which shells, and what they carry.
///
/// Three copies per shell, the same rolling-plus-pinned doctrine as
/// [`DistLayout`]:
///
/// - `path` — rolling, republished every run, embeds the rolling manifest URL.
/// - `snapshots` — named by the digest of its own patched bytes, embeds the
///   manifest *snapshot* URL, so one URL pins the script, the site values and
///   the ocx release set together.
/// - `versions` — the snapshot's bytes again, at a path named by the ocx
///   version the manifest calls latest, so a user can ask for "the installer
///   that installs 0.6.3". Re-pointed when the site values change, like a
///   cascade tag.
///
/// The defaults ride the tree `dist sync` already writes: the rolling copies
/// sit at the root beside `dist.json`, the version copies beside that
/// version's archives (derived from `publish.layout`). A GitLab generic
/// package registry has no route for a root file, exactly as for `dist.json`,
/// and overrides `path` the same way.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct InstallersLayout {
    /// Rolling copy, over `{filename}` / `{shell}`. Defaults to `{filename}`.
    #[serde(default = "default_installer_path")]
    pub path: String,

    /// Pinned copy, over `{sha256}` plus `{filename}` / `{shell}`, or `false`.
    /// Defaults to `install/{sha256}/{filename}`.
    #[serde(default)]
    pub snapshots: TemplateSwitch,

    /// Per-ocx-version copy, over `{version}` / `{tag}` plus `{filename}` /
    /// `{shell}`, or `false`. Defaults to `publish.layout` with the installer's
    /// filename — beside that version's archives.
    #[serde(default)]
    pub versions: TemplateSwitch,

    /// Which installers to publish. Defaults to all five.
    #[serde(default = "Shell::all")]
    pub shells: Vec<Shell>,

    /// Where each upstream installer is fetched from, over `{shell}` /
    /// `{filename}`. Defaults to setup.ocx.sh's stored stable pointer.
    #[serde(default = "default_installer_source")]
    pub source: String,

    /// Embedded as `OCX_INSTALL_CA_BUNDLE`: a path on the installing machine,
    /// or the PEM text itself — `{file: …}` inlines a local file so nothing
    /// has to be distributed beside the script.
    #[cfg_attr(
        feature = "jsonschema",
        schemars(with = "Option<crate::value_source::ValueSourceSchema>")
    )]
    pub ca_bundle: Option<ValueSource>,

    /// Embedded as `OCX_MANAGED_CONFIG`, the managed-config OCI reference.
    #[cfg_attr(
        feature = "jsonschema",
        schemars(with = "Option<crate::value_source::ValueSourceSchema>")
    )]
    pub managed_config: Option<ValueSource>,
}

/// A template with an on/off switch: `true` (the default template), `false`,
/// or a template string.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum TemplateSwitch {
    Switch(bool),
    Template(String),
}

impl Default for TemplateSwitch {
    fn default() -> Self {
        Self::Switch(true)
    }
}

impl Default for InstallersLayout {
    fn default() -> Self {
        Self {
            path: default_installer_path(),
            snapshots: TemplateSwitch::default(),
            versions: TemplateSwitch::default(),
            shells: Shell::all(),
            source: default_installer_source(),
            ca_bundle: None,
            managed_config: None,
        }
    }
}

fn default_installer_path() -> String {
    DEFAULT_INSTALLER_PATH.to_string()
}

fn default_installer_source() -> String {
    DEFAULT_INSTALLER_SOURCE.to_string()
}

/// One of the five installer dialects setup.ocx.sh publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Sh,
    Pwsh,
    Nu,
    Fish,
    Elvish,
}

impl Shell {
    fn all() -> Vec<Shell> {
        vec![Shell::Sh, Shell::Pwsh, Shell::Nu, Shell::Fish, Shell::Elvish]
    }

    /// The public URL word — `/sh`, `/pwsh` on setup.ocx.sh.
    #[must_use]
    pub fn segment(self) -> &'static str {
        match self {
            Shell::Sh => "sh",
            Shell::Pwsh => "pwsh",
            Shell::Nu => "nu",
            Shell::Fish => "fish",
            Shell::Elvish => "elvish",
        }
    }

    /// The canonical artifact name, `install.<ext>`.
    #[must_use]
    pub fn filename(self) -> &'static str {
        match self {
            Shell::Sh => "install.sh",
            Shell::Pwsh => "install.ps1",
            Shell::Nu => "install.nu",
            Shell::Fish => "install.fish",
            Shell::Elvish => "install.elv",
        }
    }
}

/// [`Publish::installers`] with every switch and default applied.
#[derive(Debug)]
pub struct InstallerDocs<'a> {
    /// Rolling template.
    pub path: String,
    /// Snapshot template, `None` when switched off.
    pub snapshots: Option<String>,
    /// Version template, `None` when switched off.
    pub versions: Option<String>,
    /// Deduplicated, in declaration order.
    pub shells: Vec<Shell>,
    /// Upstream URL template.
    pub source: String,
    pub ca_bundle: Option<&'a ValueSource>,
    pub managed_config: Option<&'a ValueSource>,
}

/// [`Publish::dist`] with every switch applied: where each document is
/// written, and whether each is uploaded.
///
/// Paths are always present because the tree is always written — a disabled
/// document keeps its default path there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistDocs {
    /// Relative path of the rolling manifest.
    pub path: String,
    /// Unparsed snapshot template; validated at spec load.
    pub snapshots: String,
    /// Whether the rolling manifest is uploaded.
    pub upload_path: bool,
    /// Whether the snapshot is uploaded.
    pub upload_snapshots: bool,
}

impl Publish {
    /// Resolve [`Self::dist`] into concrete paths and upload switches.
    #[must_use]
    pub fn dist_docs(&self) -> DistDocs {
        let default = DistLayout::default();
        let (layout, upload) = match &self.dist {
            DistPublish::Switch(on) => (&default, *on),
            DistPublish::Layout(layout) => (layout, true),
        };
        let (snapshots, upload_snapshots) = match &layout.snapshots {
            Snapshots::Switch(on) => (DEFAULT_SNAPSHOT_LAYOUT.to_string(), upload && *on),
            Snapshots::Template(template) => (template.clone(), upload),
        };
        DistDocs {
            path: layout.path.clone(),
            snapshots,
            upload_path: upload,
            upload_snapshots,
        }
    }
}

impl Publish {
    /// Resolve [`Self::installers`]: `None` when switched off.
    ///
    /// # Errors
    ///
    /// A message for a `versions` default that cannot be derived from
    /// `layout` — a layout naming `{target}` or `{channel}` has no single
    /// directory per version for the installer to sit in.
    pub fn installer_docs(&self) -> Result<Option<InstallerDocs<'_>>, String> {
        let default = InstallersLayout::default();
        let layout = match &self.installers {
            InstallersPublish::Switch(false) => return Ok(None),
            InstallersPublish::Switch(true) => &default,
            InstallersPublish::Layout(layout) => layout.as_ref(),
        };

        let snapshots = match &layout.snapshots {
            TemplateSwitch::Switch(false) => None,
            TemplateSwitch::Switch(true) => Some(DEFAULT_INSTALLER_SNAPSHOTS.to_string()),
            TemplateSwitch::Template(template) => Some(template.clone()),
        };
        let versions = match &layout.versions {
            TemplateSwitch::Switch(false) => None,
            TemplateSwitch::Switch(true) => {
                if self.layout.contains("{target}") || self.layout.contains("{channel}") {
                    return Err(format!(
                        "publish.installers.versions: cannot be derived from publish.layout {:?} — it names \
                         {{target}} or {{channel}}, so a version has no single directory to put an installer \
                         in; set `versions:` to a template, or `false`",
                        self.layout
                    ));
                }
                Some(self.layout.clone())
            }
            TemplateSwitch::Template(template) => Some(template.clone()),
        };

        let mut shells = Vec::with_capacity(layout.shells.len());
        for shell in &layout.shells {
            if !shells.contains(shell) {
                shells.push(*shell);
            }
        }

        Ok(Some(InstallerDocs {
            path: layout.path.clone(),
            snapshots,
            versions,
            shells,
            source: layout.source.clone(),
            // A `true` switch has no values; only the block carries them.
            ca_bundle: match &self.installers {
                InstallersPublish::Layout(layout) => layout.ca_bundle.as_ref(),
                InstallersPublish::Switch(_) => None,
            },
            managed_config: match &self.installers {
                InstallersPublish::Layout(layout) => layout.managed_config.as_ref(),
                InstallersPublish::Switch(_) => None,
            },
        }))
    }
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
    /// one to [`MirrorError::SpecInvalid`](ocx_mirror_error::MirrorError::SpecInvalid)
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

        if let Err(error) = crate::layout::LayoutTemplate::parse(&self.publish.layout) {
            errors.push(format!("publish.layout: {error}"));
        }
        let docs = self.publish.dist_docs();
        if let Err(error) = crate::layout::check_plain_path(&docs.path) {
            errors.push(format!("publish.dist.path: {error}"));
        }
        if let Err(error) = crate::layout::SnapshotTemplate::parse(&docs.snapshots) {
            errors.push(format!("publish.dist.snapshots: {error}"));
        }

        self.validate_installers(&mut errors);

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

    /// Every `publish.installers` rule.
    fn validate_installers(&self, errors: &mut Vec<String>) {
        let docs = match self.publish.installer_docs() {
            Ok(Some(docs)) => docs,
            Ok(None) => return,
            Err(error) => {
                errors.push(error);
                return;
            }
        };

        // An installer embeds a manifest URL; publishing it while the manifest
        // it names is switched off ships an installer that 404s on first use.
        let dist = self.publish.dist_docs();
        if !dist.upload_path {
            errors.push(
                "publish.installers: needs the rolling manifest the installers point at — \
                 `publish.dist` is switched off"
                    .to_string(),
            );
        }
        if !dist.upload_snapshots && (docs.snapshots.is_some() || docs.versions.is_some()) {
            errors.push(
                "publish.installers: the pinned and per-version installers point at the manifest snapshot, \
                 but `publish.dist.snapshots` is false; set `installers.snapshots: false` and \
                 `installers.versions: false`, or turn snapshots back on"
                    .to_string(),
            );
        }

        for (field, template, kind) in [
            ("path", Some(&docs.path), InstallerTemplateKind::Rolling),
            ("snapshots", docs.snapshots.as_ref(), InstallerTemplateKind::Snapshot),
            ("versions", docs.versions.as_ref(), InstallerTemplateKind::Version),
        ] {
            if let Some(template) = template
                && let Err(error) = InstallerTemplate::parse(template, kind, true)
            {
                errors.push(format!("publish.installers.{field}: {error}"));
            }
        }

        if docs.shells.is_empty() {
            errors.push(
                "publish.installers.shells: name at least one shell — an empty list publishes nothing".to_string(),
            );
        }

        // Checked as the URL the first shell renders to: the placeholders are
        // closed-set path components, so every shell has the same scheme and
        // host.
        match InstallerTemplate::parse(&docs.source, InstallerTemplateKind::Rolling, false).and_then(|template| {
            template.expand(&InstallerValues {
                filename: Shell::Sh.filename(),
                shell: Shell::Sh.segment(),
                sha256: "",
                version: "",
                tag: "",
            })
        }) {
            Ok(rendered) => match Url::parse(&rendered) {
                Ok(url) => self.validate_transport("publish.installers.source", &url, errors),
                Err(error) => errors.push(format!("publish.installers.source: {rendered:?} is not a URL: {error}")),
            },
            Err(error) => errors.push(format!("publish.installers.source: {error}")),
        }

        if let Some(value) = docs.ca_bundle {
            value.validate("publish.installers.ca_bundle", errors);
        }
        if let Some(value) = docs.managed_config {
            value.validate("publish.installers.managed_config", errors);
        }
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
    /// `pipeline::dist_sync::mirrored_url` concatenates
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
