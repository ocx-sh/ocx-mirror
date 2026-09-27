// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `publish.installers` — site-patched copies of the setup.ocx.sh installers.
//!
//! The five installers carry an embedded configuration block of four
//! `@OCX_*@` placeholders, spelled identically in every dialect and each
//! single-quoted, so the value is never interpolated and may span lines
//! (`www-setup/README.md`, "Corporate mirrors: one patched installer"). An
//! unreplaced placeholder is ignored by the installer, so a key the spec does
//! not set is simply left alone.
//!
//! Two phases, split around the archive pass so each failure lands where it
//! costs least:
//!
//! - [`prepare`] runs **before** any archive moves: it resolves the site
//!   values, fetches every upstream installer and checks each carries the
//!   placeholders about to be filled. A broken upstream or a bad value then
//!   fails the run in seconds, not after a multi-gigabyte transfer.
//! - [`Prepared::write`] runs **after** the manifest is written, because the
//!   pinned copy embeds the manifest snapshot's URL and is named by the digest
//!   of the bytes that result.
//!
//! The mirror URL placeholder is never filled: every manifest row this run
//! writes already names the mirror, which is what `OCX_INSTALL_MIRROR_URL`
//! would otherwise be for.

use std::path::Path;

use sha2::{Digest as _, Sha256};
use url::Url;

use super::manifest::DistManifest;
use super::{mirrored_url, write_output};
use ocx_mirror_error::MirrorError;
use ocx_mirror_spec::layout::{InstallerTemplate, InstallerTemplateKind, InstallerValues};
use ocx_mirror_spec::{InstallerDocs, Shell};

/// Ceiling on one upstream installer body (CWE-400). The largest dialect is
/// ~40 KB; the cap is for a hostile or broken endpoint, not for growth.
const INSTALLER_FETCH_CEILING: usize = 1024 * 1024;

const DIST_URL_TOKEN: &str = "@OCX_INSTALL_DIST_URL@";
const CA_BUNDLE_TOKEN: &str = "@OCX_INSTALL_CA_BUNDLE@";
const MANAGED_CONFIG_TOKEN: &str = "@OCX_MANAGED_CONFIG@";

/// Everything [`prepare`] established, ready to render once the manifest
/// snapshot exists.
#[derive(Debug)]
pub struct Prepared {
    installers: Vec<Upstream>,
    snapshots: Option<InstallerTemplate>,
    /// `(version, tag)` of the manifest's stable pointer, when it has one.
    latest: Option<(String, String)>,
    ca_bundle: Option<String>,
    managed_config: Option<String>,
}

/// One upstream installer, fetched and checked.
#[derive(Debug)]
struct Upstream {
    shell: Shell,
    text: String,
    /// Rendered rolling path — fixed before any byte is written, so it can be
    /// claimed against the archive layout.
    rolling_path: String,
    /// Rendered version path, when versions are on and the manifest has a
    /// stable release.
    version_path: Option<String>,
}

/// What was written for one shell — the report row and the upload plan.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InstallerReport {
    pub shell: &'static str,
    /// Rolling copy, below `output:` and `publish.base_url`.
    pub path: String,
    /// Pinned copy, when enabled and the run published a manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Per-ocx-version copy, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Resolve the site values, fetch and check each upstream installer, and
/// render every path that does not depend on the installer's own digest.
///
/// # Errors
///
/// [`MirrorError::SpecInvalid`] for a template or a site value the installers
/// cannot carry, [`MirrorError::SourceError`] when a value or an installer
/// cannot be fetched, and [`MirrorError::ExecutionFailed`] for an upstream
/// installer that does not carry a placeholder this run fills.
pub async fn prepare(
    client: &reqwest::Client,
    docs: &InstallerDocs<'_>,
    manifest: &DistManifest,
    spec_dir: &Path,
) -> Result<Prepared, MirrorError> {
    // Already validated at spec load; parsed again because a template is a
    // value, not a validation result.
    let parse = |field: &str, template: &str, kind| {
        InstallerTemplate::parse(template, kind, true)
            .map_err(|error| MirrorError::SpecInvalid(vec![format!("publish.installers.{field}: {error}")]))
    };
    let rolling = parse("path", &docs.path, InstallerTemplateKind::Rolling)?;
    let snapshots = docs
        .snapshots
        .as_deref()
        .map(|template| parse("snapshots", template, InstallerTemplateKind::Snapshot))
        .transpose()?;
    let versions = docs
        .versions
        .as_deref()
        .map(|template| parse("versions", template, InstallerTemplateKind::Version))
        .transpose()?;
    let source = InstallerTemplate::parse(&docs.source, InstallerTemplateKind::Rolling, false)
        .map_err(|error| MirrorError::SpecInvalid(vec![format!("publish.installers.source: {error}")]))?;

    let ca_bundle = resolve_value(docs.ca_bundle, "publish.installers.ca_bundle", spec_dir).await?;
    let managed_config = resolve_value(docs.managed_config, "publish.installers.managed_config", spec_dir).await?;
    // A reference is one line; a newline in one is a paste error that would
    // otherwise surface as an unparseable `--managed-config` on every machine
    // the installer runs on.
    if managed_config
        .as_deref()
        .is_some_and(|value| value.contains(['\n', '\r']))
    {
        return Err(MirrorError::SpecInvalid(vec![
            "publish.installers.managed_config: an OCI reference is a single line".to_string(),
        ]));
    }

    // The stable pointer names the release a version copy installs. A manifest
    // `select:` left without a stable release has no such copy to publish.
    let latest = manifest.latest.as_ref().and_then(|pointer| {
        manifest
            .releases
            .iter()
            .find(|row| row.version == pointer.version)
            .map(|row| (row.version.clone(), row.tag.clone()))
    });
    if versions.is_some() && latest.is_none() {
        tracing::warn!("the manifest has no stable release, so no per-version installer is published");
    }

    let mut installers = Vec::with_capacity(docs.shells.len());
    for shell in &docs.shells {
        let values = values(*shell, "", latest.as_ref());
        let url = source
            .expand(&values)
            .map_err(|error| MirrorError::SpecInvalid(vec![format!("publish.installers.source: {error}")]))?;
        let text = fetch_installer(client, &url).await?;

        let mut required = vec![DIST_URL_TOKEN];
        required.extend(ca_bundle.as_ref().map(|_| CA_BUNDLE_TOKEN));
        required.extend(managed_config.as_ref().map(|_| MANAGED_CONFIG_TOKEN));
        if let Some(token) = required.into_iter().find(|token| !text.contains(token)) {
            return Err(MirrorError::ExecutionFailed(vec![format!(
                "the {} installer from {url} carries no {token} placeholder, so the value set for it would \
                 be silently dropped; the upstream installer contract has changed",
                shell.segment()
            )]));
        }

        let render = |template: &InstallerTemplate| {
            template
                .expand(&values)
                .map_err(|error| MirrorError::ExecutionFailed(vec![error.to_string()]))
        };
        let version_path = match (&versions, &latest) {
            (Some(template), Some(_)) => Some(render(template)?),
            _ => None,
        };
        installers.push(Upstream {
            shell: *shell,
            rolling_path: render(&rolling)?,
            version_path,
            text,
        });
    }

    Ok(Prepared {
        installers,
        snapshots,
        latest,
        ca_bundle,
        managed_config,
    })
}

impl Prepared {
    /// Every path fixed before the manifest exists, with a label naming the
    /// shell — what the caller claims against the archive layout.
    pub fn claims(&self) -> impl Iterator<Item = (&str, String)> {
        self.installers.iter().flat_map(|installer| {
            let label = |kind: &str| format!("the {kind} {} installer", installer.shell.segment());
            std::iter::once((installer.rolling_path.as_str(), label("rolling"))).chain(
                installer
                    .version_path
                    .as_deref()
                    .map(|path| (path, label("per-version"))),
            )
        })
    }

    /// Report rows for a run that stops before rendering (`--dry-run`).
    pub fn planned(&self) -> Vec<InstallerReport> {
        self.installers
            .iter()
            .map(|installer| InstallerReport {
                shell: installer.shell.segment(),
                path: installer.rolling_path.clone(),
                snapshot: None,
                version: installer.version_path.clone(),
            })
            .collect()
    }

    /// Render and write every copy under `output:`.
    ///
    /// The rolling copy embeds the rolling manifest URL; the pinned copy (and
    /// the per-version copy, which is the same bytes) embeds the snapshot URL,
    /// so one pinned URL fixes the script, the site values and the release set.
    ///
    /// # Errors
    ///
    /// [`MirrorError::IndexWriteError`] for a failed write,
    /// [`MirrorError::ExecutionFailed`] for a URL that cannot be composed.
    pub async fn write(
        &self,
        output: &Path,
        base_url: &Url,
        dist_path: &str,
        snapshot_path: &str,
    ) -> Result<Vec<InstallerReport>, MirrorError> {
        let compose = |relative: &str| {
            mirrored_url(base_url, relative)
                .map(|url| url.to_string())
                .map_err(|error| MirrorError::ExecutionFailed(vec![error]))
        };
        let rolling_url = compose(dist_path)?;
        let snapshot_url = compose(snapshot_path)?;
        // `Url` leaves a `'` in a path segment unencoded, so a base or
        // manifest path carrying one would break every dialect's quoting.
        for url in [&rolling_url, &snapshot_url] {
            check_quotable("publish.base_url", url)?;
        }

        let mut reports = Vec::with_capacity(self.installers.len());
        for installer in &self.installers {
            let rolling = self.patch(installer.shell, &installer.text, &rolling_url);
            write_output(&output.join(&installer.rolling_path), rolling.as_bytes()).await?;

            let pinned_needed = self.snapshots.is_some() || installer.version_path.is_some();
            let mut snapshot = None;
            if pinned_needed {
                let pinned = self.patch(installer.shell, &installer.text, &snapshot_url);
                if let Some(template) = &self.snapshots {
                    let digest = hex::encode(Sha256::digest(pinned.as_bytes()));
                    let path = template
                        .expand(&values(installer.shell, &digest, self.latest.as_ref()))
                        .map_err(|error| MirrorError::ExecutionFailed(vec![error.to_string()]))?;
                    write_output(&output.join(&path), pinned.as_bytes()).await?;
                    snapshot = Some(path);
                }
                if let Some(path) = &installer.version_path {
                    write_output(&output.join(path), pinned.as_bytes()).await?;
                }
            }

            reports.push(InstallerReport {
                shell: installer.shell.segment(),
                path: installer.rolling_path.clone(),
                snapshot,
                version: installer.version_path.clone(),
            });
        }
        Ok(reports)
    }

    /// Fill the placeholders this run owns. Every occurrence, the way the
    /// documented `sed` does — the installers' own guard carries no complete
    /// token, so nothing else is ever rewritten.
    ///
    /// fish is the one dialect whose single quotes are not raw: `\\` and `\'`
    /// are escapes there, so a backslash is doubled for it alone — a Windows
    /// path ending in `\` would otherwise swallow the closing quote.
    fn patch(&self, shell: Shell, text: &str, dist_url: &str) -> String {
        let quote = |value: &str| match shell {
            Shell::Fish => value.replace('\\', "\\\\"),
            _ => value.to_string(),
        };
        let mut patched = text.replace(DIST_URL_TOKEN, &quote(dist_url));
        if let Some(value) = &self.ca_bundle {
            patched = patched.replace(CA_BUNDLE_TOKEN, &quote(value));
        }
        if let Some(value) = &self.managed_config {
            patched = patched.replace(MANAGED_CONFIG_TOKEN, &quote(value));
        }
        patched
    }
}

/// The placeholder values for one shell.
fn values<'a>(shell: Shell, sha256: &'a str, latest: Option<&'a (String, String)>) -> InstallerValues<'a> {
    InstallerValues {
        filename: shell.filename(),
        shell: shell.segment(),
        sha256,
        version: latest.map_or("", |(version, _)| version),
        tag: latest.map_or("", |(_, tag)| tag),
    }
}

/// Resolve one optional site value and refuse what no installer can carry.
async fn resolve_value(
    value: Option<&ocx_mirror_spec::ValueSource>,
    field: &str,
    spec_dir: &Path,
) -> Result<Option<String>, MirrorError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let resolved = value.resolve(field, spec_dir).await?;
    check_quotable(field, &resolved)?;
    Ok(Some(resolved))
}

/// Refuse a value no dialect's single-quoted configuration block can carry.
///
/// Every dialect single-quotes the value and none of them agree on how to
/// escape a quote inside one. PowerShell also closes a single-quoted string on
/// the typographic quotes U+2018–U+201B, which a pasted value can carry.
fn check_quotable(field: &str, value: &str) -> Result<(), MirrorError> {
    if value.contains(['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}']) {
        return Err(MirrorError::SpecInvalid(vec![format!(
            "{field}: the value contains a single quote, which the installers' single-quoted \
             configuration block cannot carry"
        )]));
    }
    Ok(())
}

/// Fetch one upstream installer as text.
///
/// Same leg shape as `fetch_manifest`: host-keyed credentials, the cause
/// chain in the message, and a streamed size cap.
async fn fetch_installer(client: &reqwest::Client, url: &str) -> Result<String, MirrorError> {
    let parsed = Url::parse(url).map_err(|error| MirrorError::SourceError(format!("cannot parse {url}: {error}")))?;
    let mut request = client.get(parsed.clone());
    if let Some(credential) = ocx_mirror_http::auth::resolve(&parsed)? {
        request = credential.apply(request);
    }
    let mut response = request
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| MirrorError::SourceError(format!("cannot fetch {url}: {:#}", anyhow::Error::new(error))))?;
    let bytes = ocx_mirror_http::read_capped(&mut response, url, INSTALLER_FETCH_CEILING).await?;
    String::from_utf8(bytes).map_err(|error| MirrorError::SourceError(format!("{url} is not UTF-8 text: {error}")))
}

#[cfg(test)]
#[path = "installers/tests.rs"]
mod tests;
