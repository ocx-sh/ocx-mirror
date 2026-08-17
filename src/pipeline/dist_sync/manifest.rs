// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The `dist.json` distribution manifest: parse, filter, re-point, render.
//!
//! # Why these types keep what they do not understand
//!
//! Every unknown key — top level, pointer, or release row — round-trips
//! through `#[serde(flatten)]`. A mirror that silently dropped a field
//! upstream added would break the consumers that had started reading it, and
//! the mirror is the one hop in the chain with no way to know who reads what.
//! The matching half of that contract is the consumer rule: **ignore unknown
//! keys**, which is what lets upstream add a field without breaking a pinned
//! mirror.
//!
//! # Why the rendering is hand-rolled
//!
//! `install.sh` parses this file with `grep -o '{[^{}]*}'` and `grep` is
//! line-based. Every leaf object must therefore sit on **exactly one line**,
//! and the `latest` pointer must be emitted **first**, because that script's
//! `get_latest_version` takes the first leaf object whose channel is `stable`;
//! its `dist_row` then greps a single object by version and target.
//! `serde_json::to_string_pretty` would satisfy neither, and the failure mode
//! is a jq-free installer that silently resolves nothing.
//!
//! The functions are named rather than cited by line: they live in the
//! `ocx-sh/www-setup` repository, which has no shared CI with this one, so a
//! line reference here rots at that repository's first unrelated edit. The
//! contract is pinned from this side instead — `test_dist_sync.py` runs that
//! same `grep`/`sed` pipeline against the bytes this module emits.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// The only manifest schema this mirror knows how to re-emit.
pub const SUPPORTED_SCHEMA: u64 = 1;

/// The channel string upstream gives released versions.
const STABLE_CHANNEL: &str = "stable";
/// The channel string upstream gives prereleases.
const NEXT_CHANNEL: &str = "next";

/// A parsed `dist.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DistManifest {
    pub schema: u64,
    /// Newest stable release, or `null` when the channel is empty. Serialized
    /// unconditionally — upstream emits an explicit `null` and a consumer
    /// distinguishing "absent" from "empty" would read the two differently.
    pub latest: Option<ChannelPointer>,
    pub latest_next: Option<ChannelPointer>,
    /// Newest-first, and within a release sorted by target. Filtering only
    /// ever *removes* rows, so both orderings survive without a re-sort.
    pub releases: Vec<ReleaseRow>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A `latest` / `latest_next` pointer.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChannelPointer {
    pub version: String,
    pub channel: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One release × build target.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReleaseRow {
    pub version: String,
    pub channel: String,
    pub tag: String,
    pub target: String,
    pub filename: String,
    /// Bare lowercase hex, no `sha256:` prefix. Never recomputed by the
    /// mirror — it is upstream's assertion about the bytes, and re-deriving it
    /// from what we downloaded would verify the copy against itself.
    pub sha256: String,
    pub url: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ReleaseRow {
    /// `<version>/<target>` — what identifies a row to an operator, in the
    /// report table and in every message naming this row.
    ///
    /// Control characters are escaped (CWE-117). Both halves come off a
    /// foreign manifest and reach a CI log an operator reads, and the default
    /// `publish.layout` substitutes neither of them — so `LayoutTemplate`'s
    /// refusal, which is where every *other* foreign value gets checked, never
    /// sees these two. `escape_debug` rather than `{:?}` on the pair: the
    /// quotes `{:?}` adds around each half make the common case unreadable to
    /// buy nothing.
    pub fn label(&self) -> String {
        let escaped = |value: &str| value.chars().flat_map(char::escape_debug).collect::<String>();
        format!("{}/{}", escaped(&self.version), escaped(&self.target))
    }

    /// The row's digest as an OCI digest string, for
    /// [`verify_digest`](crate::pipeline::verify::verify_digest).
    ///
    /// # Errors
    ///
    /// A message naming the row when `sha256` is not 64 hex characters. The
    /// check is here rather than at the verifier because a malformed digest
    /// means a malformed *manifest*, and the run should say so before it
    /// spends a download finding out.
    pub fn digest(&self) -> Result<String, String> {
        if self.sha256.len() != 64 || !self.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "release {}: sha256 {:?} is not 64 hex characters",
                self.label(),
                self.sha256
            ));
        }
        Ok(format!("sha256:{}", self.sha256.to_ascii_lowercase()))
    }

    /// Refuse a row whose `url` is not something this run will fetch over
    /// HTTP. `file://` and friends would make a manifest able to read the
    /// mirror host's own filesystem.
    ///
    /// # Errors
    ///
    /// A message naming the row and the offending scheme.
    pub fn check_url(&self) -> Result<url::Url, String> {
        let parsed = url::Url::parse(&self.url)
            .map_err(|error| format!("release {}: url {:?}: {error}", self.label(), self.url))?;
        match parsed.scheme() {
            "http" | "https" => Ok(parsed),
            scheme => Err(format!(
                "release {}: url scheme {scheme:?} is not http or https",
                self.label()
            )),
        }
    }
}

impl DistManifest {
    /// Drop every release the spec's `select:` rejects, then re-point
    /// `latest` / `latest_next` at what survived.
    ///
    /// Re-pointing runs on **every** call, not only when a filter is set: an
    /// upstream pointer naming a release whose rows are all absent is exactly
    /// as broken whether the mirror or the generator dropped them, and one
    /// code path cannot forget the case the other handles.
    pub fn apply_select(&mut self, select: &crate::spec::Select) {
        if let Some(min) = &select.min_version {
            // Fail-open on an unrelatable pair, matching `versions:` and the
            // per-platform bounds: `version_cmp` returning `None` means no
            // comparator understood both sides, and guessing would drop a
            // release the operator never excluded.
            self.releases
                .retain(|row| crate::filter::version_cmp(&row.version, min) != Some(Ordering::Less));
        }

        self.latest = repoint(self.latest.take(), &self.releases, STABLE_CHANNEL);
        self.latest_next = repoint(self.latest_next.take(), &self.releases, NEXT_CHANNEL);
    }

    /// Render the manifest in the shape the jq-free installers can parse:
    /// every leaf object on one line, `latest` before `latest_next` before
    /// `releases`.
    ///
    /// # Errors
    ///
    /// Whatever `serde_json` returns for a value that cannot serialize.
    pub fn render(&self) -> Result<String, serde_json::Error> {
        let mut out = String::from("{\n");
        out.push_str(&format!("  \"schema\": {},\n", self.schema));
        out.push_str(&format!("  \"latest\": {},\n", render_pointer(self.latest.as_ref())?));
        out.push_str(&format!(
            "  \"latest_next\": {},\n",
            render_pointer(self.latest_next.as_ref())?
        ));

        out.push_str("  \"releases\": [");
        if self.releases.is_empty() {
            out.push(']');
        } else {
            out.push('\n');
            for (index, row) in self.releases.iter().enumerate() {
                let separator = if index + 1 == self.releases.len() { "" } else { "," };
                out.push_str(&format!("    {}{separator}\n", serde_json::to_string(row)?));
            }
            out.push_str("  ]");
        }

        // Unknown top-level keys ride along after `releases`, never before
        // it — an extra object emitted above the pointers would become the
        // first leaf object `get_latest_version` sees.
        for (key, value) in &self.extra {
            out.push_str(&format!(
                ",\n  {}: {}",
                serde_json::to_string(key)?,
                serde_json::to_string(value)?
            ));
        }

        out.push_str("\n}\n");
        Ok(out)
    }
}

/// The pointer for `channel` after filtering.
///
/// Keeps the upstream pointer object when it still names the newest survivor,
/// so any field upstream carries on it survives the round trip. Builds a fresh
/// one otherwise, and yields `None` when the channel emptied.
fn repoint(existing: Option<ChannelPointer>, releases: &[ReleaseRow], channel: &str) -> Option<ChannelPointer> {
    // Newest-first ordering is upstream's, and filtering only removes rows, so
    // the first match is the newest survivor.
    let newest = releases.iter().find(|row| row.channel == channel)?;

    match existing {
        Some(pointer) if pointer.version == newest.version && pointer.channel == newest.channel => Some(pointer),
        _ => Some(ChannelPointer {
            version: newest.version.clone(),
            channel: newest.channel.clone(),
            extra: serde_json::Map::new(),
        }),
    }
}

/// A pointer as one line, or the literal `null`.
fn render_pointer(pointer: Option<&ChannelPointer>) -> Result<String, serde_json::Error> {
    match pointer {
        Some(pointer) => serde_json::to_string(pointer),
        None => Ok("null".to_string()),
    }
}

#[cfg(test)]
#[path = "manifest/tests.rs"]
mod tests;
