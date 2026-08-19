// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `registry.yml` — the whole-registry mirror spec (`ocx-mirror registry sync`).
//!
//! A different root type from [`MirrorSpec`](crate::spec::MirrorSpec), not a
//! variant of it: a package mirror describes one upstream tool, a registry
//! mirror describes a copy of whole index sources into a corporate registry.
//! The `kind:` discriminator that tells the two apart is read only by the
//! pre-scan ([`crate::spec::pre_scan`]), never by this type.
//!
//! Contracts C-001…C-004 and C-006 of `plan_registry_mirror_sync.md`.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use ocx_lib::oci::Identifier;
use ocx_lib::oci::ssrf::host_is_trusted;
use serde::Deserialize;
use url::Url;

use super::Target;
use crate::pipeline::registry_sync::catalog::index_host;
use crate::pipeline::registry_sync::destination::DestinationTemplate;
use crate::pipeline::registry_sync::glob::Glob;

/// Default in-flight blob count for one package's copy (C-003).
const DEFAULT_MAX_BLOBS: usize = 4;
/// Default reactive-retry budget per blob or manifest transfer (C-003).
const DEFAULT_MAX_RETRIES: u32 = 3;

/// The `registry.yml` root document (C-001).
///
/// `target` is shared verbatim with [`MirrorSpec`](crate::spec::MirrorSpec) —
/// same registry + repository pair, same meaning. `destination` is the
/// per-package repository template (C-011); `output` is the directory the
/// servable index tree is written into.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySpec {
    /// Destination registry and the repository prefix every copied package
    /// lands under. The prefix is a containment boundary, not a hint (C-013).
    pub target: Target,

    /// Directory the servable index tree is written into. Each source gets
    /// its own `<output>/<as>/` subtree (`config.json`, `c/`, `p/`).
    pub output: PathBuf,

    /// Destination repository template over `{registry}`, `{namespace}` and
    /// `{package}` — plain substitution, no template engine (C-011).
    pub destination: String,

    /// Whether the mirrored index points at [`Self::target`], or keeps the
    /// `repository` pointer the source published.
    ///
    /// **Default `false`** — preserve. The mirrored root then names the
    /// upstream host, and clients reach the copy through ocx's own
    /// `[mirrors."<host>"]` map, which rewrites transport only: package
    /// identity, digests and cache keys stay keyed upstream. That is the
    /// deployment an artifact-manager remote already implements, and it is why
    /// this is the default rather than the opt-in.
    ///
    /// `true` restores the self-describing tree: every root points at
    /// `target`, and a client needs no `[mirrors]` entry at all — at the cost
    /// of re-homing every package onto the corporate host.
    ///
    /// It governs **only** the published pointer. `target`, `destination` and
    /// the expansion template decide where the artifacts land under either
    /// value; keeping that path reachable through the client's `[mirrors]`
    /// prefix is the operator's job, and
    /// [`mirror_path_mismatch`](crate::pipeline::registry_sync::destination::mirror_path_mismatch)
    /// warns when it provably is not.
    #[serde(default)]
    pub rewrite_pointers: bool,

    /// Whether the copy creates the upstream tag set at the destination, or
    /// pushes the content and leaves it untagged.
    ///
    /// **Default `true`.** A client resolving through the mirrored index never
    /// reads a destination tag — the index root maps every tag to a `content`
    /// digest and the pull is by digest — so the tags exist for the humans and
    /// the tools that address the registry directly, and to keep the content
    /// referenced.
    ///
    /// That second job is the reason this defaults on and is not merely a
    /// performance knob. An untagged manifest is unreferenced, and a registry
    /// is free to garbage-collect it: zot does by default, and an Artifactory
    /// cleanup policy can be configured to. Turning this off on such a
    /// destination publishes an index naming content the registry may delete
    /// underneath it.
    ///
    /// Turn it off when the destination keeps untagged manifests and the tag
    /// set is large: one tag is one `PUT`, and an ocx package routinely
    /// carries two or three cascade tags per version.
    #[serde(default = "default_publish_tags")]
    pub publish_tags: bool,

    /// What a per-package failure does to the rest of the run. Governs
    /// per-package failures only; a non-authoritative destination read aborts
    /// the run under either value (C-040).
    #[serde(default)]
    pub on_error: OnError,

    /// The index sources copied by this spec, in the order they are processed.
    pub sources: Vec<RegistrySource>,

    #[serde(default)]
    pub concurrency: RegistryConcurrency,
}

impl RegistrySpec {
    /// Every validation rule of C-006, one message per violated rule.
    ///
    /// Returns an empty vector for a valid spec; the caller maps a non-empty
    /// one to [`MirrorError::SpecInvalid`](crate::error::MirrorError::SpecInvalid)
    /// (exit 65). Reports every violation rather than the first, so an
    /// operator fixes one spec once.
    ///
    /// `spec_path` is accepted so this reads exactly like
    /// [`MirrorSpec::validate`](crate::spec::MirrorSpec::validate) at every
    /// call site — the loader and `tests/registry_spec_validation.rs` drive
    /// both the same way. It is deliberately unread: no rule below is
    /// path-relative, and nothing on disk is opened.
    pub fn validate(&self, _spec_path: &Path) -> Vec<String> {
        let mut errors = Vec::new();

        if self.sources.is_empty() {
            errors.push("sources: at least one source is required".to_string());
        }

        self.validate_target(&mut errors);
        self.validate_destination(&mut errors);
        self.validate_sources(&mut errors);

        if self.output.as_os_str().is_empty() {
            errors.push("output: a directory to write the index tree into is required".to_string());
        }

        if self.concurrency.max_blobs == 0 {
            errors.push("concurrency.max_blobs: must be at least 1 — zero blobs in flight copies nothing".to_string());
        }

        errors
    }

    /// The destination prefix, refused rather than normalised.
    ///
    /// Same doctrine as
    /// [`physical_repository`](crate::pipeline::registry_sync::destination::physical_repository):
    /// `target.repository` is the containment boundary every copied package
    /// lands under, so a prefix the OCI grammar would reject must fail here,
    /// at spec load, rather than half way through a multi-hour copy.
    fn validate_target(&self, errors: &mut Vec<String>) {
        if let Err(error) = Identifier::validate_repository(&self.target.repository) {
            errors.push(format!(
                "target.repository: '{}' is not a legal OCI repository path: {}",
                self.target.repository, error.kind
            ));
            // The registry check below parses the composed `registry/repository`,
            // so a bad repository would surface twice, the second time against a
            // field that is not at fault.
            return;
        }

        // Checked on the composed reference because that is the string every
        // consumer parses, and because a registry carrying a path segment
        // (`ghcr.io/ocx-contrib`) parses perfectly well — it just silently
        // moves that segment into the repository, which is why the parsed
        // registry has to be compared back against the configured one.
        match Identifier::parse_with_default_registry(&self.target.reference(), &self.target.registry) {
            Ok(identifier) if identifier.registry() == self.target.registry => {}
            Ok(identifier) => errors.push(format!(
                "target.registry: '{}' is not a bare registry host — '{}' parses with registry '{}'",
                self.target.registry,
                self.target.reference(),
                identifier.registry()
            )),
            Err(error) => errors.push(format!(
                "target.registry: '{}' does not form a parseable reference with target.repository: {}",
                self.target.registry, error.kind
            )),
        }
    }

    fn validate_destination(&self, errors: &mut Vec<String>) {
        let template = match DestinationTemplate::parse(&self.destination) {
            Ok(template) => template,
            Err(error) => {
                errors.push(format!("destination: {error}"));
                return;
            }
        };

        // With one source every package is already distinguished by its own
        // catalog key. With two, `{registry}` is the only thing keeping two
        // sources' same-named packages apart — without it every such pair
        // reaches C-015 as a collision instead of being impossible by
        // construction.
        if self.sources.len() > 1 && !template.uses_registry() {
            errors.push(format!(
                "destination: '{}' must contain {{registry}} when the spec lists more than one source, \
                 or two sources publishing the same package name land on one repository",
                self.destination
            ));
        }
    }

    fn validate_sources(&self, errors: &mut Vec<String>) {
        let mut claimed: HashMap<&str, usize> = HashMap::new();

        for (index, source) in self.sources.iter().enumerate() {
            let name = source.as_name();

            if let Some(reason) = as_name_error(name) {
                errors.push(format!(
                    "sources[{index}].as: '{name}' is not a legal OCI path component ({reason}); \
                     set `as:` to a name usable as one path segment"
                ));
            }

            if let Some(first) = claimed.insert(name, index) {
                errors.push(format!(
                    "sources[{index}].as: '{name}' is already used by sources[{first}] — each source needs its \
                     own `as:`, which names both its output subtree and its {{registry}} expansion"
                ));
            }

            if let Some(reason) = index_transport_error(&source.index, &source.trusted_hosts) {
                errors.push(format!("sources[{index}].index: {reason}"));
            }

            for (field, patterns) in [("include", &source.include), ("exclude", &source.exclude)] {
                for (pattern_index, pattern) in patterns.iter().enumerate() {
                    if let Err(error) = Glob::compile(pattern) {
                        errors.push(format!("sources[{index}].{field}[{pattern_index}]: {error}"));
                    }
                }
            }
        }
    }
}

/// Why a source's `index:` may not be fetched over the scheme it declares, or
/// `None` (C-006).
///
/// **`https`, unless the host appears in that source's `trusted_hosts`.** The
/// index tree is the mirror's *control plane*: `c/index.json` and the roots
/// name every package, `content` digest and `repository` a run will copy, so an
/// on-path attacker — in scope per the threat model — rewrites the whole plan
/// by editing one plaintext response. Digest verification cannot save it,
/// because the digests come out of the same tampered document.
/// [`build_source_index_client`](crate::pipeline::registry_sync::catalog::build_source_index_client)
/// already refuses a *redirect* downgrade; this is the same rule applied to the
/// configured scheme, which nothing else inspects.
///
/// The exemption is `trusted_hosts` rather than a flag of its own because it is
/// the same escape hatch, judged on the same host spelling ([`index_host`]),
/// that already opens an RFC1918 corporate index and the acceptance harness's
/// `http://localhost:5001` past the SSRF floor. One list, one decision.
///
/// A URL that does not parse, or carries no host, is deliberately **not** this
/// rule's refusal:
/// [`validate_index_base_host`](crate::pipeline::registry_sync::catalog) refuses
/// it before the first fetch, and reporting one defect twice against two fields
/// helps nobody.
fn index_transport_error(index: &str, trusted_hosts: &[String]) -> Option<String> {
    let url = Url::parse(index).ok()?;
    if url.scheme() == "https" {
        return None;
    }

    let host = index_host(&url)?;
    if host_is_trusted(&host, trusted_hosts) {
        return None;
    }

    Some(format!(
        "'{}' is a plaintext transport, and this source's index is the control plane naming every \
         package and digest the run copies; use https, or add '{host}' to this source's \
         `trusted_hosts:`",
        url.scheme()
    ))
}

/// Why `as_name` cannot serve as one OCI path component, or `None`.
///
/// `as:` is both the served subtree name under `output:` and the `{registry}`
/// expansion, so it has to survive as a single path segment. A `:` is the
/// trap — copying `registry: localhost:5001` into `as:` is the obvious thing
/// to do and produces a value no repository path can hold — while a `.`
/// (`ocx.sh`, `ghcr.io`) is perfectly legal.
fn as_name_error(as_name: &str) -> Option<String> {
    if as_name.contains('/') {
        return Some("a path separator makes it more than one component".to_string());
    }
    Identifier::validate_repository(as_name)
        .err()
        .map(|error| error.kind.to_string())
}

/// One upstream index source (C-002).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySource {
    /// The OCI registry the source's content is pulled from.
    pub registry: String,

    /// Base URL of the source's published index tree — the document root that
    /// serves `config.json`, `c/index.json` and `p/<ns>/<pkg>.json`.
    pub index: String,

    /// Subtree name under `output:` **and** the `{registry}` expansion. Falls
    /// back to `registry` verbatim, never slugified (see [`Self::as_name`]).
    ///
    /// Immutable after first publish: it is both the served subtree name and
    /// the destination repository segment, so renaming it re-homes every
    /// destination repository *and* breaks every consumer pointing
    /// `[registries]` at `<output>/<as>`.
    #[serde(rename = "as")]
    pub as_name: Option<String>,

    /// Package-name globs selecting what to copy. Empty selects everything.
    #[serde(default)]
    pub include: Vec<String>,

    /// Package-name globs vetoing what to copy. An exclude beats an include
    /// unconditionally (C-010).
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Hosts exempted from the SSRF floor for this source, modelled on
    /// `announce`'s field of the same name. Not optional polish: a corporate
    /// registry on an RFC1918 address is refused without it, which is the
    /// motivating deployment.
    #[serde(default)]
    pub trusted_hosts: Vec<String>,
}

impl RegistrySource {
    /// The subtree name and `{registry}` expansion for this source — `as:` if
    /// set, otherwise `registry` **verbatim**.
    ///
    /// Never slugified: the value is a served path segment operators point
    /// `[registries]` at, so it must read back exactly as written.
    pub fn as_name(&self) -> &str {
        self.as_name.as_deref().unwrap_or(&self.registry)
    }
}

/// Copy-concurrency knobs for `registry sync` (C-003).
///
/// Deliberately **not** [`ConcurrencyConfig`](crate::spec::ConcurrencyConfig):
/// that type's `max_downloads` defaults to 8 and three of its five knobs have
/// no meaning on this path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConcurrency {
    /// Blobs in flight for one package. Peak resident memory is
    /// `max_blobs × largest_blob`, because the copy buffers each blob whole
    /// to verify its digest before the first byte is pushed.
    #[serde(default = "default_max_blobs")]
    pub max_blobs: usize,

    /// Extra attempts after a reactive 429 / `Retry-After`. No proactive rate
    /// limiting.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

impl Default for RegistryConcurrency {
    fn default() -> Self {
        Self {
            max_blobs: DEFAULT_MAX_BLOBS,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

fn default_publish_tags() -> bool {
    true
}

fn default_max_blobs() -> usize {
    DEFAULT_MAX_BLOBS
}

fn default_max_retries() -> u32 {
    DEFAULT_MAX_RETRIES
}

/// What a per-package failure does to the rest of the run (C-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OnError {
    /// Count the failure, report it, keep going. The run still exits non-zero.
    #[default]
    Continue,
    /// Abort at the first per-package failure.
    FailFast,
}

#[cfg(test)]
#[path = "registry/tests.rs"]
mod tests;
