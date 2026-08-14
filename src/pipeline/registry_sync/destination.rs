// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Where a copied package lands: template expansion, the OCI grammar guard,
//! prefix containment, the `oci://` pointer, and collision detection.
//!
//! This is the destination trust boundary. Catalog keys come off a foreign
//! index, so every rule here **refuses**; nothing normalises. Contracts
//! C-011…C-015 of `plan_registry_mirror_sync.md`.
//!
//! **Every refusal quotes the offending key with `{:?}`, never bare** (CWE-117).
//! A key reaches [`DestinationTemplate::expand`] straight off the source's
//! catalog, before any charset guard has judged it: a key holding a newline
//! forges log lines in the CI output an operator reads, and `\u{202e}` or an
//! ANSI escape reaches their terminal. `char::escape_debug` escapes both
//! classes and supplies the quoting these messages already implied.

use ocx_lib::oci::Identifier;
use ocx_lib::oci::index::parse_physical_repository;

use crate::error::MirrorError;
use crate::spec::Target;

/// A parsed `destination:` template (C-011).
///
/// Plain substitution over exactly three placeholders — `{registry}`,
/// `{namespace}`, `{package}`. No template engine: three names do not justify
/// a dependency, and a closed set is what lets an unknown placeholder be an
/// error instead of an empty string.
#[derive(Debug, Clone)]
pub struct DestinationTemplate {
    /// The template as written — the value's identity in a `Debug` line, and
    /// the capacity hint [`Self::expand`] sizes its buffer from.
    source: String,
    /// The alternating literal/placeholder decomposition.
    segments: Vec<TemplateSegment>,
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateSegment {
    Literal(String),
    Registry,
    Namespace,
    Package,
}

/// Why a `destination:` template or a catalog key was refused (C-011, C-012).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TemplateError {
    /// A `{…}` naming something other than the three known placeholders.
    UnknownPlaceholder { name: String },
    /// A literal `{` that never closes.
    UnterminatedPlaceholder { template: String },
    /// A catalog key that is not `<namespace>/<package>`: no `/`, an empty
    /// segment, or more than two segments. Refused, never repaired.
    MalformedCatalogKey { key: String },
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPlaceholder { name } => write!(
                f,
                "unknown placeholder '{{{name}}}' in destination template; the known placeholders are \
                 {{registry}}, {{namespace}} and {{package}}"
            ),
            Self::UnterminatedPlaceholder { template } => {
                write!(f, "unterminated '{{' in destination template '{template}'")
            }
            Self::MalformedCatalogKey { key } => {
                write!(f, "catalog key {key:?} is not '<namespace>/<package>'")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

impl DestinationTemplate {
    /// Parse a `destination:` template (C-011).
    ///
    /// # Errors
    ///
    /// [`TemplateError::UnknownPlaceholder`] naming the offending placeholder,
    /// or [`TemplateError::UnterminatedPlaceholder`] for a `{` that opens no
    /// known placeholder.
    pub fn parse(template: &str) -> Result<DestinationTemplate, TemplateError> {
        let mut segments = Vec::new();
        let mut rest = template;

        while let Some(open) = rest.find('{') {
            let (literal, after_open) = (&rest[..open], &rest[open + 1..]);
            let Some(close) = after_open.find('}') else {
                return Err(TemplateError::UnterminatedPlaceholder {
                    template: template.to_string(),
                });
            };
            let placeholder = match &after_open[..close] {
                "registry" => TemplateSegment::Registry,
                "namespace" => TemplateSegment::Namespace,
                "package" => TemplateSegment::Package,
                unknown => {
                    return Err(TemplateError::UnknownPlaceholder {
                        name: unknown.to_string(),
                    });
                }
            };
            if !literal.is_empty() {
                segments.push(TemplateSegment::Literal(literal.to_string()));
            }
            segments.push(placeholder);
            rest = &after_open[close + 1..];
        }

        if !rest.is_empty() {
            segments.push(TemplateSegment::Literal(rest.to_string()));
        }

        Ok(DestinationTemplate {
            source: template.to_string(),
            segments,
        })
    }

    /// Whether `{registry}` appears — the predicate behind C-006's
    /// multi-source rule, which is what stops two sources from colliding by
    /// construction.
    pub fn uses_registry(&self) -> bool {
        self.segments.contains(&TemplateSegment::Registry)
    }

    /// Expand for one package (C-012).
    ///
    /// `catalog_key` is the **package name** (the catalog key, `<ns>/<pkg>`),
    /// never the physical `repository` — the two diverge for every indirected
    /// package, and expanding the physical form would re-home the mirror onto
    /// whatever path the upstream registry happens to use.
    ///
    /// The key's shape is checked whether or not this template substitutes it.
    /// A template ignoring `{namespace}`/`{package}` would otherwise collapse
    /// every malformed key onto one destination instead of naming the source's
    /// catalog as broken.
    ///
    /// # Errors
    ///
    /// [`TemplateError::MalformedCatalogKey`] for a key with no `/`, an empty
    /// segment, or more than two segments.
    pub fn expand(&self, registry_as: &str, catalog_key: &str) -> Result<String, TemplateError> {
        let (namespace, package) = split_catalog_key(catalog_key)?;

        let mut expanded = String::with_capacity(self.source.len() + registry_as.len() + catalog_key.len());
        for segment in &self.segments {
            match segment {
                TemplateSegment::Literal(text) => expanded.push_str(text),
                TemplateSegment::Registry => expanded.push_str(registry_as),
                TemplateSegment::Namespace => expanded.push_str(namespace),
                TemplateSegment::Package => expanded.push_str(package),
            }
        }
        Ok(expanded)
    }
}

/// Split a catalog key into `(namespace, package)` — refuse, never repair.
///
/// Exactly two non-empty segments. `split_once` takes the *first* `/`, so a
/// third segment lands inside `package` and is caught by the `contains('/')`
/// check rather than being silently absorbed.
fn split_catalog_key(key: &str) -> Result<(&str, &str), TemplateError> {
    let malformed = || TemplateError::MalformedCatalogKey { key: key.to_string() };
    let (namespace, package) = key.split_once('/').ok_or_else(malformed)?;
    if namespace.is_empty() || package.is_empty() || package.contains('/') {
        return Err(malformed());
    }
    Ok((namespace, package))
}

/// Compose and validate the destination repository for one package (C-013).
///
/// `format!("{}/{}", target.repository, expanded)`, then:
///
/// 1. `Identifier::validate_repository` — **refuse, never normalise**.
///    Uppercase is refused rather than lowercased, because `str::to_lowercase`
///    folds U+212A KELVIN SIGN onto `k` and would manufacture collisions.
///    `.` and `..` segments are refused as directory traversal.
/// 2. Containment: the composed value must start with
///    `"{target.repository}/"`, checked **after** validation.
///
/// # Errors
///
/// [`MirrorError::SpecInvalid`] (exit 65) at plan time, for a key that fails
/// the OCI repository grammar or escapes the configured prefix.
pub fn physical_repository(target: &Target, expanded: &str) -> Result<String, MirrorError> {
    let composed = format!("{}/{}", target.repository, expanded);

    // The whole string is validated, not a decomposition of it: `Identifier::parse`
    // splits a tag and digest off *before* its repository guards run, so a
    // key of `ns/pkg:latest` would parse as the repository `ns/pkg` while the
    // mirror went on to use the whole thing. `validate_repository` is the
    // entry point that applies every guard to the string as given.
    Identifier::validate_repository(&composed).map_err(|error| {
        MirrorError::SpecInvalid(vec![format!(
            "destination repository {composed:?} is not a legal OCI repository path: {}",
            error.kind
        )])
    })?;

    // Containment is asserted on the *result*, never on the key. Composing by
    // `format!` makes it structurally true today, so this cannot fire — which
    // is the point: it is the invariant the composition owes, stated where it
    // will fail loudly if the composition ever stops guaranteeing it.
    let prefix = format!("{}/", target.repository);
    if !composed.starts_with(&prefix) {
        return Err(MirrorError::SpecInvalid(vec![format!(
            "destination repository {composed:?} escapes the configured prefix '{}'",
            target.repository
        )]));
    }

    Ok(composed)
}

/// The `oci://` pointer written into the mirrored root's `repository` field
/// (C-014).
///
/// Returns `format!("oci://{}/{}", target.registry, physical)` and validates it
/// by calling `parse_physical_repository` on the composed string **before
/// returning** — so the mirror reaches the same verdict every consumer will,
/// rather than shipping a pointer that only fails at resolve time.
///
/// # Errors
///
/// [`MirrorError::SpecInvalid`] (exit 65) when the composed pointer does not
/// round-trip through `parse_physical_repository`.
pub fn wire_pointer(target: &Target, physical: &str) -> Result<String, MirrorError> {
    let pointer = format!("oci://{}/{}", target.registry, physical);

    parse_physical_repository(&pointer).map_err(|error| {
        MirrorError::SpecInvalid(vec![format!(
            "destination pointer is not a legal index repository value: {error}"
        )])
    })?;

    Ok(pointer)
}

/// One catalog key's destination, as the plan phase resolved it.
///
/// Carries the source's `as:` name because the catalog key alone is not unique
/// across sources: two sources may both publish `kitware/cmake`, and a
/// collision message naming only the keys would name the same string twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// The source's `as:` value — its `{registry}` expansion and output
    /// subtree.
    pub source: String,
    /// The catalog key, exactly as that source's catalog spells it.
    pub catalog_key: String,
    /// The physical repository the key expanded to, already validated and
    /// contained by [`physical_repository`].
    pub repository: String,
}

/// Refuse a run in which two distinct catalog keys expand to the same physical
/// repository (C-015).
///
/// A whole-run property, checked once over the union of every source's
/// expanded set — per source, source 1 would already be fully mirrored before
/// source 2's expansion revealed the clash. Plain string equality, no case
/// normalisation anywhere, because uppercase was already refused by
/// [`physical_repository`].
///
/// Distinctness is the `(source, catalog_key)` pair: the same package seen
/// twice is a duplicate, not a collision.
///
/// # Errors
///
/// [`MirrorError::SpecInvalid`] (exit 65) at plan time, one message per
/// collision, each naming **both** colliding sides and the repository they
/// share.
pub fn detect_collisions(expansions: &[Expansion]) -> Result<(), MirrorError> {
    let mut claimed: std::collections::HashMap<&str, &Expansion> = std::collections::HashMap::new();
    let mut collisions = Vec::new();

    for expansion in expansions {
        match claimed.get(expansion.repository.as_str()) {
            None => {
                claimed.insert(&expansion.repository, expansion);
            }
            Some(first) => {
                if (&first.source, &first.catalog_key) == (&expansion.source, &expansion.catalog_key) {
                    continue;
                }
                collisions.push(format!(
                    "destination collision on {:?}: source '{}' package {:?} and source '{}' package {:?}",
                    expansion.repository, first.source, first.catalog_key, expansion.source, expansion.catalog_key
                ));
            }
        }
    }

    if collisions.is_empty() {
        Ok(())
    } else {
        Err(MirrorError::SpecInvalid(collisions))
    }
}

#[cfg(test)]
#[path = "destination/tests.rs"]
mod tests;
