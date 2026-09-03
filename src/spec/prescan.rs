// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Raw-value pre-scan for every spec root — `mirror.yml`, `registry.yml`
//! and `dist.yml` (C-005).
//!
//! Runs on the **merged** document, before typed deserialization — which is
//! the whole point. A credential in an `extends:` base is caught with no
//! chain-walking of its own, and `kind:` is readable at all: `RegistrySpec`
//! carries `deny_unknown_fields` and no `kind` field, so a check *after*
//! deserialization is unreachable in exactly the case it exists for — a
//! `mirror.yml` handed to `registry sync` fails on `sources:` first and buries
//! the real problem.
//!
//! Every rejection here is
//! [`MirrorError::SpecUsageError`](crate::error::MirrorError::SpecUsageError)
//! → exit 64, matching `policy_check_notify`'s precedent of running before
//! `validate()` so a policy violation is 64 rather than 65.

use std::path::Path;

use serde_yaml_ng::Value;

use crate::error::MirrorError;

/// Keys refused at any depth, in any document this pre-scan sees.
///
/// Comparison is case-insensitive ASCII on the key alone — the value is never
/// inspected, never compared, and never echoed.
///
/// **Known limit, ruled deliberate:** a credential parked under an innocuous
/// key name (`{name: token, value: hunter2}`) is not caught, because the
/// deny-list reads key names only. Catching it needs a value-shape heuristic,
/// which forces this function to *read values* to decide — and every value
/// read is a leak surface in the one place whose whole guarantee is that no
/// value ever reaches an error message. Pinned by
/// `tests::credentials::a_credential_under_an_innocuous_key_name_is_a_known_limit`.
pub const CREDENTIAL_DENY_LIST: &[&str] = &[
    "password",
    "token",
    "username",
    "auth",
    "credentials",
    "secret",
    "api_key",
];

/// The top-level discriminator key. Named here so WP-08's loader strips the
/// same key it was read from, alongside `extends:`.
pub const KIND_KEY: &str = "kind";

/// The `kind:` a `registry.yml` must declare.
pub const REGISTRY_KIND: &str = "registry";

/// The `kind:` a `dist.yml` must declare.
pub const DIST_KIND: &str = "dist";

/// The remedy clause every credential rejection ends with.
///
/// `<slug>` stays a literal placeholder: deriving the real slug would mean
/// reading a value out of the document, which is the one thing these messages
/// must never do.
const CREDENTIAL_REMEDY: &str = "set OCX_AUTH_<slug>_TOKEN in the environment instead";

/// Refuse a merged spec document that carries credentials, declares the
/// wrong `kind:`, or hides userinfo in a source index URL (C-005).
///
/// Three jobs, in this order:
///
/// 1. **Credential deny-list** — any key in [`CREDENTIAL_DENY_LIST`], at any
///    depth, whatever the value's type.
/// 2. **`kind` discriminator** — checked only when `expected_kind` is
///    `Some`; absent, non-string, or not `expected_kind` at the top level
///    is a rejection. `None` skips this job entirely — `mirror.yml` (the
///    `load_spec` caller) carries no `kind:` field, so demanding one would
///    reject every existing spec (C-053/C-007).
/// 3. **`sources[].index` userinfo** — a URL whose authority carries a
///    non-empty userinfo component.
///
/// **No message this function produces contains any *value* from the
/// document** — only key paths, the spec file name, and the name of the
/// environment variable to set instead. That negative property is the
/// security-relevant half: naming the offending key while echoing its value
/// would leak the secret into every log the run writes.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`](crate::error::MirrorError::SpecUsageError)
/// (exit 64) for every rejection above, first violation wins.
pub fn pre_scan(merged: &Value, spec_path: &Path, expected_kind: Option<&str>) -> Result<(), MirrorError> {
    scan_for_credentials(merged, "", spec_path)?;
    if let Some(expected_kind) = expected_kind {
        check_kind(merged, spec_path, expected_kind)?;
    }
    check_source_index_userinfo(merged, spec_path)
}

/// Walk every mapping and sequence, refusing the first credential-shaped key.
///
/// `path` is the dotted key path of `value`'s parent, empty at the root;
/// sequence elements append `[index]`.
fn scan_for_credentials(value: &Value, path: &str, spec_path: &Path) -> Result<(), MirrorError> {
    match value {
        Value::Mapping(map) => {
            for (key, child) in map {
                let name = key.as_str().unwrap_or_default();
                let child_path = if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path}.{name}")
                };
                if CREDENTIAL_DENY_LIST
                    .iter()
                    .any(|denied| name.eq_ignore_ascii_case(denied))
                {
                    return Err(MirrorError::SpecUsageError(format!(
                        "{}: {child_path}: credentials must not appear in a spec; {CREDENTIAL_REMEDY}",
                        spec_path.display()
                    )));
                }
                scan_for_credentials(child, &child_path, spec_path)?;
            }
            Ok(())
        }
        Value::Sequence(items) => {
            for (index, item) in items.iter().enumerate() {
                scan_for_credentials(item, &format!("{path}[{index}]"), spec_path)?;
            }
            Ok(())
        }
        // A `!tag` is transparent to serde, which untags and accepts the
        // contents — so leaving it opaque here would hide a whole subtree
        // from the deny-list. The tag itself is never a key, so the path is
        // unchanged by descending through it.
        Value::Tagged(tagged) => scan_for_credentials(&tagged.value, path, spec_path),
        _ => Ok(()),
    }
}

/// Absent, non-string and wrong-value `kind:` are one rejection: the document
/// is not a registry spec, and which way it is wrong tells the operator
/// nothing they cannot see in their own file.
fn check_kind(merged: &Value, spec_path: &Path, expected_kind: &str) -> Result<(), MirrorError> {
    match merged
        .as_mapping()
        .and_then(|map| map.get(KIND_KEY))
        .and_then(Value::as_str)
    {
        Some(kind) if kind == expected_kind => Ok(()),
        _ => Err(MirrorError::SpecUsageError(format!(
            "{}: {KIND_KEY}: a {expected_kind} spec must declare `{KIND_KEY}: {expected_kind}`",
            spec_path.display()
        ))),
    }
}

/// Refuse `https://user:pass@host/` in any source's `index:`.
fn check_source_index_userinfo(merged: &Value, spec_path: &Path) -> Result<(), MirrorError> {
    let Some(sources) = merged
        .as_mapping()
        .and_then(|map| map.get("sources"))
        .and_then(Value::as_sequence)
    else {
        return Ok(());
    };

    for (index, source) in sources.iter().enumerate() {
        let Some(url) = source
            .as_mapping()
            .and_then(|map| map.get("index"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        // ponytail: a string that does not parse as a URL is not a working
        // index base — `catalog::validate_index_base_host` is the sole
        // downstream guard and refuses it before the first fetch (nothing in
        // `RegistrySpec::validate` inspects `index:` beyond its scheme) — and
        // only a URL that would actually be dialled can leak a credential.
        let Ok(parsed) = url::Url::parse(url) else {
            continue;
        };
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(MirrorError::SpecUsageError(format!(
                "{}: sources[{index}].index: the URL must not embed credentials; {CREDENTIAL_REMEDY}",
                spec_path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "prescan/tests.rs"]
mod tests;
