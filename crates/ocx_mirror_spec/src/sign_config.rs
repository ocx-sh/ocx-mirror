// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `sign:` — the mirror's own signing identity for what it produces (D1).
//!
//! A present `sign:` carries exactly one mode tag, `keyless {...}` xor
//! `key: <ref>` / `key: {...}`, and every value under it is a [`Ref`]: a
//! literal, `env://NAME`, or `file://PATH`. This module owns the shape
//! only; every C-051 refusal (the mutual-exclusion rule, an empty `key`,
//! a secret-class field given a literal, ...) lives in
//! [`validate_sign_config`](super::validate_sign_config) in `validate.rs`,
//! mirroring where `policy_check_notify` sits relative to
//! [`NotifyConfig`](super::NotifyConfig).

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize, de};

/// A signing-config value: a literal, `env://NAME`, or `file://PATH`.
///
/// Parsed once at deserialization by splitting on the `env://`/`file://`
/// prefix. That split is real code, not a stub, but it deliberately
/// **never rejects a value** — an empty `NAME` or an empty `PATH` still
/// parses as `Env(String::new())`/`File(PathBuf::new())`. Every C-051
/// refusal (an `env://` `NAME` failing `^[A-Z_][A-Z0-9_]*$`, an empty
/// `file://` path, a literal disguised as a secret, `key: {}`, ...) is
/// enforced by [`validate_sign_config`](super::validate_sign_config)
/// instead, so a rejection can name the *field* the offending `Ref` sits
/// under — a property this type alone cannot express. This is the plan's
/// stated exception to the usual parse-don't-validate newtype shape
/// (`architecture.md` ARCH-04: the contract here is the plan's, not the
/// generic rule).
///
/// Holds a *reference* — an environment variable name or a file path —
/// never a resolved secret value, so `derive(Debug)` is sound *once
/// [`validate_sign_config`](super::validate_sign_config) has refused a
/// literal secret-class value*. Between deserialization and that refusal
/// a `Ref::Literal` may still hold a literal an operator wrongly wrote for
/// `passphrase`/`identity_token`; nothing may log a spec in that window.
///
/// `Serialize` is kept on this type alone in the module — unlike
/// [`SignConfig`]/[`KeylessConfig`]/[`KeyConfig`]/[`KeyFullConfig`] — because
/// its `env://`/`file://` round-trip through `From<Ref> for String` is
/// exactly the string WP 2 hands to ocx's `--key`/`--fulcio-url`/
/// `--rekor-url` flags. Never `--passphrase`: the two secret-class refs
/// (`passphrase`, `identity_token`) reach the child through its environment
/// (C-054), so no secret is ever an argv word. A [`Ref::File`] renders as
/// `file://PATH`; whether that scheme is stripped before the flag is WP 2's
/// call, against ocx's own key-reference grammar.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Ref {
    /// A literal value, taken verbatim. Refused by `validate_sign_config`
    /// for a secret-class field (`passphrase`, `identity_token`).
    Literal(String),
    /// `env://NAME` — resolved from the named environment variable.
    Env(String),
    /// `file://PATH` — resolved by reading the named file.
    File(PathBuf),
}

/// Redacts the literal arm, so `{:?}` on a spec is safe *before* validation
/// rather than only after it.
///
/// A `Ref::Literal` legitimately holds a path or a URL, but between
/// deserialization and [`validate_sign_config`](super::validate_sign_config)
/// it may also hold a secret an operator wrongly wrote for `passphrase` or
/// `identity_token` — and a derived `Debug` would print it (API-02). `Env`
/// and `File` name where a value lives, never the value, so both show.
impl fmt::Debug for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(<redacted>)"),
            Self::Env(name) => write!(f, "Env({name})"),
            Self::File(path) => write!(f, "File({})", path.display()),
        }
    }
}

impl From<String> for Ref {
    /// The prefix split never refuses a value: an empty `NAME`/`PATH`
    /// still parses. See the type's own doc comment for why refusal is
    /// deferred to `validate_sign_config` instead of a `TryFrom`.
    fn from(value: String) -> Self {
        if let Some(name) = value.strip_prefix("env://") {
            Self::Env(name.to_string())
        } else if let Some(path) = value.strip_prefix("file://") {
            Self::File(PathBuf::from(path))
        } else {
            Self::Literal(value)
        }
    }
}

impl From<Ref> for String {
    fn from(value: Ref) -> Self {
        match value {
            Ref::Literal(literal) => literal,
            Ref::Env(name) => format!("env://{name}"),
            // LOSSY-OK: record — path came from a UTF-8 YAML scalar, so
            // display() cannot lose bytes here.
            Ref::File(path) => format!("file://{}", path.display()),
        }
    }
}

/// Keyless (Sigstore/Fulcio) signing config under `sign.keyless`.
///
/// `keyless: {}` means public Sigstore. Every field is resolved and
/// emitted as `--fulcio-url`/`--rekor-url` (WP 2, C-052) from the
/// mirror-owned `DEFAULT_FULCIO_URL`/`DEFAULT_REKOR_URL` constants when
/// omitted here — **never** through ocx's own `[trust.sigstore]`, which
/// is per-machine and consumer-side only (ADR D1, the amendment
/// rationale).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeylessConfig {
    /// The Fulcio instance. Defaults to `https://fulcio.sigstore.dev`.
    #[serde(default)]
    pub fulcio: Option<Ref>,
    /// The Rekor instance. Defaults to `https://rekor.sigstore.dev`.
    #[serde(default)]
    pub rekor: Option<Ref>,
    /// The OIDC identity token, for CI platforms ocx cannot auto-detect
    /// (GitHub Actions, GitLab and CircleCI resolve ambiently and leave
    /// this unset). `env://` or `file://` only — a literal is refused
    /// (C-051).
    #[serde(default)]
    pub identity_token: Option<Ref>,
}

/// `sign.key` in either the string form (`key: <ref>`) or the map form.
///
/// Deserialized by hand rather than as an `#[serde(untagged)]` enum: an
/// untagged enum reports every mismatch as "data did not match any
/// variant", which would swallow the specific `unknown field` diagnostic
/// the map form's [`KeyFullConfig`] target is meant to produce for a
/// misspelled key — the same reason
/// [`CascadeConfig`](super::CascadeConfig) hand-rolls its `Deserialize`
/// impl. `key: <ref>` is shorthand for `{ ref: <ref> }` with no
/// passphrase and no Rekor upload, matching the D1 grammar exactly. A
/// `key:` value matching neither a string nor a map (e.g. a YAML
/// sequence) surfaces the visitor's `expecting()` message naming both
/// accepted shapes; a Specify test asserts its wording (DATA-FMT-07).
///
/// The map form's fields live on [`KeyFullConfig`] rather than inline on
/// this variant: `deny_unknown_fields` is a container attribute, so it
/// has to land on the *struct* the map is deserialized into, not on this
/// enum or a bare variant of it. Routing the map form through
/// `Full(KeyFullConfig)`, reached through
/// `serde::de::value::MapAccessDeserializer`, is what makes an
/// unrecognised key under `key: {...}` surface as serde's own
/// ``unknown field `…` `` naming the key, instead of the untagged
/// "did not match any variant" message that names nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyConfig {
    /// `key: <ref>` — the bare ocx `--key` reference, no passphrase, no
    /// Rekor upload (renders `--no-rekor-upload`).
    Reference(Ref),
    /// `key: { ref, passphrase?, rekor? }`. See [`KeyFullConfig`].
    Full(KeyFullConfig),
}

impl<'de> Deserialize<'de> for KeyConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(KeyConfigVisitor)
    }
}

struct KeyConfigVisitor;

impl<'de> de::Visitor<'de> for KeyConfigVisitor {
    type Value = KeyConfig;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string `ref`, or a map with `ref` and optional `passphrase`/`rekor`")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(KeyConfig::Reference(Ref::from(value.to_string())))
    }

    fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        let full = KeyFullConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(KeyConfig::Full(full))
    }
}

/// The map form of `sign.key`: `{ ref, passphrase?, rekor? }`.
///
/// Strict — `deny_unknown_fields` on this struct rejects an unrecognised
/// key even when it is reached through [`KeyConfig`]'s hand-rolled
/// `visit_map`: the attribute is evaluated on the struct being
/// deserialized into, not on whatever dispatches to it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyFullConfig {
    /// The ocx `--key` reference. Required — `key: {}` is refused
    /// (C-051): ocx has no default signing key.
    #[serde(rename = "ref")]
    pub reference: Ref,
    /// The key's passphrase. `env://` or `file://` only — a literal
    /// is refused (C-051).
    #[serde(default)]
    pub passphrase: Option<Ref>,
    /// Present → renders `--rekor-upload --rekor-url <U>`; absent →
    /// `--no-rekor-upload`. Never inherits a fleet
    /// `[trust.sigstore].rekor_upload` (ADR D1, sub-decision 5) —
    /// silence must not push a private digest to the public log.
    #[serde(default)]
    pub rekor: Option<Ref>,
}

/// The `sign:` block.
///
/// Exactly one of `keyless`/`key` is set — enforced by
/// [`validate_sign_config`](super::validate_sign_config), not by serde:
/// "exactly one of two optional fields" has no serde-level XOR
/// expression. Absent `sign:` on [`MirrorSpec`](super::MirrorSpec) means
/// the mirror publishes unsigned, unchanged from every push leg's current
/// default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignConfig {
    /// Keyless signing. See [`KeylessConfig`].
    #[serde(default)]
    pub keyless: Option<KeylessConfig>,
    /// Key-mode signing, string or map form. See [`KeyConfig`].
    #[serde(default)]
    pub key: Option<KeyConfig>,
}

#[cfg(test)]
#[path = "sign_config/tests.rs"]
mod tests;
