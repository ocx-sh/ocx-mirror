// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Spec validation: every rule that turns a parsed document into a list of
//! diagnostics.
//!
//! Rejected documents are covered by the fixture corpus in
//! `tests/fixtures/invalid/`, one file per rule, rather than by a Rust test
//! per rule — see `tests/spec_validation.rs`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use ocx_lib::oci::Platform;
use ocx_lib::package::version::Version;

use super::platform_keys::{infer_libc_from_image, infer_shell_from_image, libc_family_feature};
use super::{
    AnnounceConfig, ContainerConfig, ExcludeEntry, KeyConfig, KeylessConfig, MirrorSpec, NotifyConfig, OcxMirrorConfig,
    PlatformConfig, Ref, SignConfig, TestEntry,
};
use super::{
    CRON_RE, DISCORD_USER_ID_RE, GHA_SECRET_NAME_RE, GIT_REV_RE, GITHUB_REPO_RE, INDEX_PACKAGE_RE, TEST_NAME_RE,
    applicability_key,
};
use crate::error::MirrorError;

/// Reject a cron expression that cannot be interpolated into a generated
/// workflow's `schedule:` block.
///
/// GitHub remains the only validator of cron *semantics* — a nonsense but
/// well-formed `99 99 * * *` still renders. The charset guard exists because
/// the value is spliced verbatim into `on:` inside a single-quoted scalar
/// (`schedule_block` in `generate/ci.rs`): a quote or newline would close that
/// scalar and let a spec add triggers of its own, and a scheduled cascade run
/// repairs for real.
pub fn validate_cron(label: &str, cron: &str, errors: &mut Vec<String>) {
    if cron.trim().is_empty() || !CRON_RE.is_match(cron) {
        errors.push(format!("{label}: invalid cron expression '{cron}'"));
    }
}

/// The `metadata:` rejection message for an env source (`pylock`/`pypi`):
/// env metadata is composed from the resolved lock, so a hand-authored
/// `metadata.json` has nothing to attach to.
pub fn metadata_not_supported_error(source_type: &str) -> String {
    format!(
        "metadata: not supported for source.type '{source_type}' \
         (env metadata is composed from the lock; use catalog:/CATALOG.md for the description)"
    )
}

/// The `bin_scan:` rejection message for an env source, shaped like
/// [`metadata_not_supported_error`]: both name a setting that only an
/// extracted archive tree could satisfy.
pub fn bin_scan_not_supported_error(source_type: &str) -> String {
    format!(
        "bin_scan: not supported for source.type '{source_type}' \
         (an env package has no extracted archive to scan; its interface comes from the lock)"
    )
}

/// Validate `tests:` entries: non-empty, unique names, valid name regex,
/// and exactly one of `command|script|script_inline` set per entry.
pub fn validate_tests(tests: &[TestEntry], errors: &mut Vec<String>) {
    if tests.is_empty() {
        errors.push("tests: must contain at least one entry".to_string());
        return;
    }

    let mut seen = HashSet::new();
    for entry in tests {
        if !TEST_NAME_RE.is_match(&entry.name) {
            errors.push(format!(
                "tests: invalid name '{}' (must match ^[a-zA-Z][a-zA-Z0-9_-]*$)",
                entry.name
            ));
        }
        if !seen.insert(&entry.name) {
            errors.push(format!("tests: duplicate name '{}'", entry.name));
        }

        // Exactly-one-of enforcement.
        let set_count = [
            entry.command.is_some(),
            entry.script.is_some(),
            entry.script_inline.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        match set_count {
            1 => {}
            0 => errors.push(format!(
                "tests: entry '{}' must set exactly one of command|script|script_inline (none set)",
                entry.name
            )),
            n => errors.push(format!(
                "tests: entry '{}' must set exactly one of command|script|script_inline ({n} set)",
                entry.name
            )),
        }
    }
}

/// Check that every `script:` a spec names exists, resolved from the repository
/// root.
///
/// `script:` is the one spec path that is repository-root-relative:
/// `metadata.default` and `catalog.*` resolve against the spec's own directory,
/// but the generated workflow runs `ocx package test --script` from the checkout
/// root. In a single-spec repository those are the same directory and the
/// asymmetry never shows; in a multi-spec one they diverge, and the natural
/// `script: tests/smoke.star` written inside `buildifier/mirror.yml` quietly
/// means `<repo>/tests/smoke.star`. A path that resolves to nothing renders
/// green here and fails as a red test leg after a publish attempt, so it is a
/// spec error (exit 65) like a missing `metadata.default`.
///
/// `spec_dir` is the spec's own directory *relative to `repo_root`* — `None` for
/// a root spec, where the near miss cannot arise.
pub fn validate_test_scripts(spec: &MirrorSpec, repo_root: &Path, spec_dir: Option<&Path>) -> Vec<String> {
    let mut errors = Vec::new();
    check_test_scripts("tests", spec.tests.as_deref(), repo_root, spec_dir, &mut errors);

    // Per-platform overrides carry scripts too, and nothing else validates them.
    if let Some(platforms) = &spec.platforms {
        let mut keys: Vec<&String> = platforms.keys().collect();
        keys.sort();
        for key in keys {
            check_test_scripts(
                &format!("platforms: '{key}': tests"),
                platforms[key].tests.as_deref(),
                repo_root,
                spec_dir,
                &mut errors,
            );
        }
    }
    errors
}

pub fn check_test_scripts(
    scope: &str,
    entries: Option<&[TestEntry]>,
    repo_root: &Path,
    spec_dir: Option<&Path>,
    errors: &mut Vec<String>,
) {
    for entry in entries.unwrap_or_default() {
        let Some(script) = &entry.script else { continue };
        let resolved = repo_root.join(script);
        if resolved.exists() {
            continue;
        }
        // Name what was resolved and against what — the author wrote a path that
        // looks right from where they were standing.
        let mut message = format!(
            "{scope}: entry '{}' script not found: {} resolves from the repository root as {}",
            entry.name,
            script.display(),
            resolved.display(),
        );
        // The near miss is the actual mistake being made, so say it outright
        // rather than leave the author to discover the asymmetry.
        if let Some(dir) = spec_dir
            && repo_root.join(dir).join(script).exists()
        {
            message.push_str(&format!(
                " — `script:` is repository-root-relative, unlike metadata.default and catalog.*, \
                 which resolve from the spec's directory; write {}",
                dir.join(script).display(),
            ));
        }
        errors.push(message);
    }
}

/// Validate a single `exclude` entry: exactly one of single-`version` or a
/// `min_version`/`max_version` range, and any present version parses.
pub fn validate_exclude_entry(key: &str, index: usize, entry: &ExcludeEntry, errors: &mut Vec<String>) {
    let has_version = entry.version.is_some();
    let has_range = entry.min_version.is_some() || entry.max_version.is_some();

    if !has_version && !has_range {
        errors.push(format!(
            "platforms: '{key}': exclude[{index}] must set 'version' or a 'min_version'/'max_version' range"
        ));
    }
    if has_version && has_range {
        errors.push(format!(
            "platforms: '{key}': exclude[{index}] cannot set both 'version' and a 'min_version'/'max_version' range"
        ));
    }
    for (field, value) in [
        ("version", &entry.version),
        ("min_version", &entry.min_version),
        ("max_version", &entry.max_version),
    ] {
        if let Some(raw) = value {
            match Version::parse(raw) {
                None => errors.push(format!(
                    "platforms: '{key}': exclude[{index}] {field} '{raw}' is not a valid version"
                )),
                // Match keys on the release core, so a variant/build-stamped
                // bound would compare asymmetrically — require a plain version.
                Some(parsed) if applicability_key(&parsed) != parsed => errors.push(format!(
                    "platforms: '{key}': exclude[{index}] {field} '{raw}' must be a plain version without a variant prefix or build metadata"
                )),
                Some(_) => {}
            }
        }
    }
    // An inverted exclude range (min ≥ max) matches nothing — a silent no-op. Reject it.
    if let Some(min_raw) = entry.min_version.as_ref()
        && let Some(max_raw) = entry.max_version.as_ref()
        && let Some(min) = Version::parse(min_raw)
        && let Some(max) = Version::parse(max_raw)
        && applicability_key(&min) >= applicability_key(&max)
    {
        errors.push(format!(
            "platforms: '{key}': exclude[{index}] min_version '{min_raw}' must be below max_version '{max_raw}'"
        ));
    }
}

/// Validate one container's `setup:` list.
///
/// Each entry becomes a single Dockerfile `RUN`, passed to the container's
/// shell as written. Rejected here are the shapes that would not arrive as one
/// command: a list that declares nothing to run, an entry that runs nothing, an
/// entry carrying a newline (the natural `script_inline`-style mistake, which
/// splits one `RUN` into a broken Dockerfile), and an entry ending in a
/// backslash — a line continuation, which quietly absorbs the *next* `RUN` and
/// leaves that layer unbuilt while the build still exits 0.
pub fn validate_container_setup(key: &str, container: &ContainerConfig, errors: &mut Vec<String>) {
    let Some(setup) = &container.setup else {
        return;
    };
    let image = &container.image;
    if setup.is_empty() {
        errors.push(format!(
            "platforms: '{key}': container image '{image}' declares an empty setup list; \
             drop the key or give it at least one command"
        ));
    }
    for (index, command) in setup.iter().enumerate() {
        if command.trim().is_empty() {
            errors.push(format!(
                "platforms: '{key}': container image '{image}': setup[{index}] must not be blank"
            ));
        } else if command.contains('\n') {
            errors.push(format!(
                "platforms: '{key}': container image '{image}': setup[{index}] must be a single \
                 command (each entry becomes one Dockerfile RUN); split it across entries"
            ));
        } else if command.trim_end().ends_with('\\') {
            // Trimmed first: docker continues the line on a backslash that is
            // the last *non-whitespace* character, so trailing spaces do not
            // save it. Either way `RUN foo \` swallows the next `RUN` as its
            // own arguments — the build exits 0 having skipped that layer,
            // leaving the leg green on an unprovisioned image.
            errors.push(format!(
                "platforms: '{key}': container image '{image}': setup[{index}] must not end with \
                 a backslash; it would continue into the next RUN instead of ending the command"
            ));
        }
    }
}

/// Validate `platforms:` map: valid platform keys, runner present, container
/// image format, shell defaults for known distros, explicit shell required for
/// unknown, per-container `setup:` commands, plus per-platform version
/// applicability (`min_version`, `max_version`, `exclude`).
pub fn validate_platforms(platforms: &HashMap<String, PlatformConfig>, errors: &mut Vec<String>) {
    for (key, config) in platforms {
        // The canonical `os/arch[/variant][+feature,…]` grammar, parsed by the
        // same `FromStr` the `assets:` keys and every `--platform` flag use. A
        // hand-rolled regex here is what kept `linux/amd64+libc.musl` — the only
        // way to declare a libc claim — out of the test matrix entirely.
        let parsed = key.parse::<Platform>();
        if parsed.is_err() {
            errors.push(format!(
                "platforms: invalid key '{key}' (must be os/arch[+feature] format, \
                 e.g. linux/amd64 or linux/amd64+libc.musl)"
            ));
        }

        if config.runner.trim().is_empty() {
            errors.push(format!("platforms: '{key}': runner must not be empty"));
        }

        for (field, value) in [
            ("min_version", &config.min_version),
            ("max_version", &config.max_version),
        ] {
            if let Some(raw) = value {
                match Version::parse(raw) {
                    None => errors
                        .push(format!("platforms: '{key}': {field} '{raw}' is not a valid version")),
                    // Applicability compares on the release core (build stamp and
                    // variant prefix stripped via `applicability_key`); a bound
                    // carrying either would compare asymmetrically and silently
                    // misfilter, so require a plain version here.
                    Some(parsed) if applicability_key(&parsed) != parsed => errors.push(format!(
                        "platforms: '{key}': {field} '{raw}' must be a plain version without a variant prefix or build metadata"
                    )),
                    Some(_) => {}
                }
            }
        }
        // An inverted window (min ≥ max) silently drops the platform from every
        // version. Reject it — min is inclusive, max exclusive, so equal is empty too.
        if let Some(min_raw) = config.min_version.as_ref()
            && let Some(max_raw) = config.max_version.as_ref()
            && let Some(min) = Version::parse(min_raw)
            && let Some(max) = Version::parse(max_raw)
            && applicability_key(&min) >= applicability_key(&max)
        {
            errors.push(format!(
                "platforms: '{key}': min_version '{min_raw}' must be below max_version '{max_raw}'"
            ));
        }
        for (index, entry) in config.exclude.iter().enumerate() {
            validate_exclude_entry(key, index, entry, errors);
        }

        if let Some(containers) = &config.containers {
            if containers.is_empty() {
                errors.push(format!(
                    "platforms: '{key}': containers must contain at least one entry when declared"
                ));
            } else {
                // Container legs are `docker run --platform <key>` on a Linux
                // runner. A macOS or Windows runner has no Linux container
                // engine at all, so the pairing can only ever fail at run time —
                // reject it while the maintainer is still looking at the spec.
                if !key.starts_with("linux/") {
                    errors.push(format!(
                        "platforms: '{key}': containers are linux-only (tests run via `docker run`)"
                    ));
                }
                // The libc family the platform key claims, if it claims one.
                // Declaring `+libc.musl` and then testing in a glibc image is
                // the silent failure this whole matrix exists to prevent: a
                // musl-static artifact runs fine under glibc, so the leg goes
                // green having verified nothing. Reject the pairing here, where
                // the maintainer is still looking at the spec.
                let declared_libc: Vec<&str> = match parsed.as_ref() {
                    Ok(Platform::Specific { os_features, .. }) => os_features
                        .iter()
                        .filter(|f| f.starts_with("libc."))
                        .map(String::as_str)
                        .collect(),
                    _ => Vec::new(),
                };

                for container in containers {
                    // If no explicit shell, the image must have a known default.
                    if container.shell.is_none() && infer_shell_from_image(&container.image).is_none() {
                        errors.push(format!(
                            "platforms: '{key}': container image '{}' has ambiguous shell; \
                             set an explicit shell (e.g. shell: bash)",
                            container.image
                        ));
                    }

                    let image_libc = libc_family_feature(infer_libc_from_image(&container.image));
                    if !declared_libc.is_empty() && !declared_libc.contains(&image_libc) {
                        errors.push(format!(
                            "platforms: '{key}': container image '{}' is {image_libc}, \
                             but the platform declares {} — the leg would run without \
                             testing the libc claim",
                            container.image,
                            declared_libc.join(",")
                        ));
                    }

                    validate_container_setup(key, container, errors);
                }
            }
        }
    }
}

/// Validate `ocx_mirror:` block: rev format.
pub fn validate_ocx_mirror_config(config: &OcxMirrorConfig, errors: &mut Vec<String>) {
    if let Some(rev) = &config.rev
        && !GIT_REV_RE.is_match(rev)
    {
        errors.push(format!(
            "ocx_mirror: rev '{rev}' must be a 40-character lowercase hex SHA"
        ));
    }
}

/// Content-policy check on the `notify:` block.
///
/// Rejects any `webhook_secret` value that looks like a hardcoded URL. This is a
/// *policy* violation (exit 64 / `SpecUsageError`), distinct from the structural
/// format check in `validate_notify_config` (exit 65 / `SpecInvalid`).
///
/// Call this from `load_spec` **before** `spec.validate()` so the correct exit code
/// is returned even when a structurally-valid spec contains a bad policy choice.
pub fn policy_check_notify(notify: &NotifyConfig) -> Result<(), MirrorError> {
    let Some(discord) = &notify.discord else {
        return Ok(());
    };
    let secret = &discord.webhook_secret;

    // R3 mitigation: reject any hardcoded URL — catches accidental paste of the raw webhook URL.
    if secret.starts_with("https://") || secret.starts_with("http://") {
        return Err(MirrorError::SpecUsageError(format!(
            "webhook_secret: hardcoded URL not allowed; use a GitHub Actions secret name instead (got '{secret}')"
        )));
    }
    if secret.contains("discord.com") || secret.contains("discordapp.com") {
        return Err(MirrorError::SpecUsageError(format!(
            "webhook_secret: value must not contain a Discord URL; use a GitHub Actions secret name instead (got '{secret}')"
        )));
    }

    // The user id is non-secret but a frequent paste mistake — catch a URL or
    // `@mention` early (exit 64) rather than letting it slip into the workflow.
    if let Some(user_id) = &discord.user_id {
        if user_id.starts_with("https://") || user_id.starts_with("http://") {
            return Err(MirrorError::SpecUsageError(format!(
                "notify.discord.user_id: hardcoded URL not allowed; use the numeric Discord user ID (got '{user_id}')"
            )));
        }
        if user_id.contains('@') {
            return Err(MirrorError::SpecUsageError(format!(
                "notify.discord.user_id: must be the numeric Discord snowflake, not an @mention (got '{user_id}')"
            )));
        }
    }

    Ok(())
}

/// Refuse a `sign:` shape that only exists on the raw document (C-051).
///
/// Runs between [`pre_scan`](super::pre_scan) and deserialization, so **every
/// refusal here precedes every refusal in [`validate_sign_config`]**: C-051's
/// listed order is read per seat, and this is the first seat. That is why
/// `sign: {keyless: {}, key: {}}` is refused for its empty `key:` map rather
/// than for naming both tags. Whatever survives deserialization is
/// `validate_sign_config`'s.
///
/// Four shapes, in this order, each exit 64 naming the field and never
/// echoing a value:
///
/// 1. a null `sign:` — `Option<SignConfig>` deserializes it to `None`,
///    indistinguishable from an absent key, so the mirror would publish
///    unsigned while the spec says otherwise (the S-051 hazard).
/// 2. a null `keyless:`/`key:` — the same erasure one level down.
///    `{keyless: null, key: env://K}` reads as plain key mode, and a lone
///    null `keyless:` reports "neither tag" against a document that names one.
/// 3. a non-string `passphrase`/`identity_token` — serde's type error is
///    exit 65 *and quotes the offending scalar*, which for those two fields
///    is the secret itself.
/// 4. a `key:` map with no `ref` — `KeyFullConfig::reference` is a required
///    field, so serde would reject the document as malformed data (65)
///    before the usage error (64) the operator needs.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`] (exit 64) for the first shape found; the
/// message names the field and the spec path, never a value.
pub fn refuse_raw_sign_shapes(merged: &serde_yaml_ng::Value, spec_path: &Path) -> Result<(), MirrorError> {
    let Some(sign) = merged.as_mapping().and_then(|map| map.get("sign")) else {
        return Ok(());
    };

    if sign.is_null() {
        return Err(raw_sign_refusal(
            spec_path,
            "sign",
            "is null; omit the key entirely to publish unsigned, or give it `keyless:`/`key:` to sign",
        ));
    }

    // A `sign:` that is neither null nor a mapping is a shape error serde
    // reports without reading a value — leave it to the typed seat.
    let Some(sign) = sign.as_mapping() else {
        return Ok(());
    };

    for tag in ["keyless", "key"] {
        if sign.get(tag).is_some_and(serde_yaml_ng::Value::is_null) {
            return Err(raw_sign_refusal(
                spec_path,
                &format!("sign.{tag}"),
                "is null; give it a value or remove the key",
            ));
        }
    }

    for (tag, secret) in [("keyless", "identity_token"), ("key", "passphrase")] {
        let field = sign
            .get(tag)
            .and_then(serde_yaml_ng::Value::as_mapping)
            .and_then(|map| map.get(secret));
        // Null is the legitimate spelling of "absent" for an `Option<Ref>`.
        if field.is_some_and(|value| !matches!(value, serde_yaml_ng::Value::String(_) | serde_yaml_ng::Value::Null)) {
            return Err(raw_sign_refusal(
                spec_path,
                &format!("sign.{tag}.{secret}"),
                "must be a quoted string reference; use `env://NAME` or `file://PATH`",
            ));
        }
    }

    if sign
        .get("key")
        .and_then(serde_yaml_ng::Value::as_mapping)
        .is_some_and(|key| !key.contains_key("ref"))
    {
        return Err(raw_sign_refusal(
            spec_path,
            "sign.key.ref",
            "a `key:` map must name the signing key; ocx has no default",
        ));
    }

    Ok(())
}

/// The raw seat's message shape: spec path, field, reason — never a value.
///
/// Prefixed with the spec path because every other refusal reaching the
/// operator from this seat is ([`pre_scan`](super::pre_scan)); the typed seat
/// is unprefixed, matching `policy_check_notify`.
fn raw_sign_refusal(spec_path: &Path, field: &str, reason: &str) -> MirrorError {
    MirrorError::SpecUsageError(format!("{}: {field}: {reason}", spec_path.display()))
}

/// Whether a [`Ref`] sits under a field whose value is a secret.
///
/// `passphrase` and `identity_token` accept only `env://`/`file://`: a
/// literal there is key material in a file an operator commits. Every other
/// field takes a literal legitimately (`key: ./cosign.key`, a Rekor URL), so
/// the distinction is per call site and cannot be read off the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefClass {
    /// A literal is a legitimate spelling here.
    Plain,
    /// A literal would inline a secret.
    Secret,
}

/// Whether `name` is a portably exportable environment variable name.
///
/// The grammar is `^[A-Z_][A-Z0-9_]*$` (C-051), spelled as a character walk
/// rather than a regex: it is the only place in the crate that needs it, and
/// an empty name falls out of the same check.
fn is_env_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_uppercase() || first == '_')
        && characters.all(|character| character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_')
}

/// Whether `name` is one of the variables ocx's plugin dispatch scrubs.
///
/// The source of truth is ocx's own list — `ocx_lib::env::keys::CREDENTIAL_KEYS`
/// (`external/ocx/crates/ocx_lib/src/env.rs:238`), the same constant
/// `app/plugin_dispatch.rs` removes from the child environment. Naming it
/// rather than copying the three strings is what keeps a rename upstream from
/// silently reopening the hole: drift there is otherwise invisible here.
fn is_dispatch_scrubbed(name: &str) -> bool {
    ocx_lib::env::keys::CREDENTIAL_KEYS.contains(&name)
}

/// Refuse one [`Ref`] under `sign:`, naming `field` and never the value.
///
/// The rules are uniform across every `Ref` in the block (C-051): an empty
/// reference names nothing, an inlined PEM body is key material in the spec,
/// a secret-class field takes no literal, an `env://` name outside the
/// grammar is one no shell can export, and an `env://` name ocx's plugin
/// dispatch scrubs is one that means two different things depending on
/// how the tool was invoked.
///
/// The scrub rule is what stops one spec meaning two things. Under
/// `ocx mirror package pipeline push` the dispatch empties those three
/// variables from the mirror's own environment *and* from the `ocx` child's,
/// so the reference resolves to nothing however it is spelled; run directly,
/// nothing scrubs them and the same reference resolves fine.
///
/// What that costs differs by seat, which is why the message claims neither:
/// `sign.key`/`sign.key.ref` reach the child verbatim as `--key <ref>`, so
/// `ocx package push --sign` tags the whole cascade before the signature
/// fails and publishes it unsigned, while `sign.key.passphrase` and
/// `sign.keyless.identity_token` are resolved by the mirror before the first
/// push and fail as `SignMaterialMissing` (exit 78) with nothing published.
/// Refusing the spec replaces both with a clean exit 64.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`] (exit 64) for the first rule the value
/// breaks; the message carries `field` and never the offending value.
fn check_sign_ref(field: &str, value: &Ref, class: RefClass) -> Result<(), MirrorError> {
    match value {
        Ref::Literal(literal) if literal.is_empty() => Err(MirrorError::SpecUsageError(format!(
            "{field}: an empty reference names nothing; give a literal, `env://NAME`, or `file://PATH`"
        ))),
        // A pasted PEM body, caught before the secret-class rule so the
        // message says what is wrong rather than only where. ponytail:
        // armour-marker match only — armourless base64 in a plain-class field
        // is not caught. The control is against an accidental commit of
        // armoured key material, not against an adversary, who is out of
        // scope here anyway (`security-threat-model.md`: the spec author is
        // inside the trusted boundary).
        Ref::Literal(literal) if literal.contains("BEGIN ") => Err(MirrorError::SpecUsageError(format!(
            "{field}: key material must not be inlined; use `env://NAME` or `file://PATH` to name where the key lives"
        ))),
        Ref::Literal(_) if class == RefClass::Secret => Err(MirrorError::SpecUsageError(format!(
            "{field}: a literal secret is refused; use `env://NAME` or `file://PATH`"
        ))),
        Ref::Env(name) if !is_env_variable_name(name) => Err(MirrorError::SpecUsageError(format!(
            "{field}: the `env://` variable name must match ^[A-Z_][A-Z0-9_]*$"
        ))),
        // The name, never the value: `name` is what the operator typed.
        // Refused unconditionally rather than only under dispatch. A direct
        // `ocx-mirror` run *can* read these — nothing scrubs them, and the
        // rendered workflows invoke the binary directly — so the hazard is
        // not unreadability but inconsistency: one spec, two meanings. The
        // message stops there rather than naming a consequence, because the
        // consequence is per-seat (see this function's doc comment). There is
        // no marker to condition on (`OCX_BINARY_PIN` is set on the scrubbing
        // and non-scrubbing paths alike), and keying off `env::var` would
        // make spec validity depend on what happens to be exported.
        Ref::Env(name) if is_dispatch_scrubbed(name) => Err(MirrorError::SpecUsageError(format!(
            "{field}: `env://{name}` names a variable reserved by ocx, whose plugin dispatch \
             strips it from the environment before launching the mirror; the same spec would resolve it under a \
             direct `ocx-mirror` run and resolve to nothing under `ocx mirror ...`, so it is refused outright. \
             Use an operator-owned name, for example `env://MIRROR_SIGNING_KEY`"
        ))),
        Ref::File(path) if path.as_os_str().is_empty() => Err(MirrorError::SpecUsageError(format!(
            "{field}: the `file://` path is empty"
        ))),
        Ref::Literal(_) | Ref::Env(_) | Ref::File(_) => Ok(()),
    }
}

/// Refuse the `sign.keyless` fields, in declaration order.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`] (exit 64) — see [`check_sign_ref`].
fn check_keyless(keyless: &KeylessConfig) -> Result<(), MirrorError> {
    if let Some(fulcio) = &keyless.fulcio {
        check_sign_ref("sign.keyless.fulcio", fulcio, RefClass::Plain)?;
    }
    if let Some(rekor) = &keyless.rekor {
        check_sign_ref("sign.keyless.rekor", rekor, RefClass::Plain)?;
    }
    if let Some(identity_token) = &keyless.identity_token {
        check_sign_ref("sign.keyless.identity_token", identity_token, RefClass::Secret)?;
    }
    Ok(())
}

/// Refuse the `sign.key` fields, in declaration order.
///
/// The string form is named `sign.key` and the map form `sign.key.ref`, so a
/// refusal points at the line the operator actually wrote.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`] (exit 64) — see [`check_sign_ref`].
fn check_key(key: &KeyConfig) -> Result<(), MirrorError> {
    match key {
        KeyConfig::Reference(reference) => check_sign_ref("sign.key", reference, RefClass::Plain),
        KeyConfig::Full(full) => {
            check_sign_ref("sign.key.ref", &full.reference, RefClass::Plain)?;
            if let Some(passphrase) = &full.passphrase {
                check_sign_ref("sign.key.passphrase", passphrase, RefClass::Secret)?;
            }
            if let Some(rekor) = &full.rekor {
                check_sign_ref("sign.key.rekor", rekor, RefClass::Plain)?;
            }
            Ok(())
        }
    }
}

/// Refuse a `sign:` block the mirror cannot honour (C-051).
///
/// Mirrors [`policy_check_notify`]'s placement and precedent: called from
/// `load_spec` *before* [`MirrorSpec::validate`] so a policy violation is
/// exit 64 (`SpecUsageError`) rather than 65 (`SpecInvalid`). Every
/// rejection names the offending field and never echoes a value:
///
/// - `sign: {}` — neither `keyless` nor `key` present.
/// - both `keyless` and `key` present — the D1 tags are mutually exclusive.
/// - a `Ref` that is empty, or whose literal form contains `"BEGIN "`
///   (a literal PEM pasted where a reference was meant).
/// - a secret-class field (`passphrase`, `identity_token`) given a
///   `Ref::Literal` — those two fields accept only `env://`/`file://`.
/// - an `env://NAME` not matching `^[A-Z_][A-Z0-9_]*$`.
/// - an `env://NAME` naming a variable ocx's plugin dispatch scrubs
///   (`ocx_lib::env::keys::CREDENTIAL_KEYS`) — readable on a direct run,
///   empty under dispatch, so the spec's meaning depends on the caller.
/// - a `file://` whose path is empty.
///
/// The shapes that never survive deserialization — a null `sign:`, a null
/// mode tag, a non-string secret, a `key:` map with no `ref` — are
/// [`refuse_raw_sign_shapes`]'s, and it runs first.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`] (exit 64) for the first violation found.
pub fn validate_sign_config(cfg: &SignConfig) -> Result<(), MirrorError> {
    // C-051's order, first violation wins: the mode tags before the fields,
    // because a block naming no mode has no fields worth reporting on.
    match (&cfg.keyless, &cfg.key) {
        (None, None) => Err(MirrorError::SpecUsageError(
            "sign: neither `keyless:` nor `key:` is set; name exactly one, or omit `sign:` to publish unsigned"
                .to_string(),
        )),
        (Some(_), Some(_)) => Err(MirrorError::SpecUsageError(
            "sign: `keyless:` and `key:` are mutually exclusive; name exactly one".to_string(),
        )),
        (Some(keyless), None) => check_keyless(keyless),
        (None, Some(key)) => check_key(key),
    }
}

/// Validate `notify:` block: webhook_secret must be a valid GHA secret name format.
///
/// URL-literal checks are handled separately by [`policy_check_notify`] with a
/// `SpecUsageError` (exit 64). This function only checks the structural format,
/// contributing to `SpecInvalid` (exit 65) errors.
pub fn validate_notify_config(config: &NotifyConfig, errors: &mut Vec<String>) {
    let Some(discord) = &config.discord else {
        return;
    };

    let secret = &discord.webhook_secret;

    // Must match GHA secret name format.
    if !GHA_SECRET_NAME_RE.is_match(secret) {
        errors.push(format!(
            "webhook_secret: '{secret}' is not a valid GitHub Actions secret name \
             (must match ^[A-Z][A-Z0-9_]+$)"
        ));
    }

    // The mention target must be a numeric Discord snowflake (17–20 digits).
    if let Some(user_id) = &discord.user_id
        && !DISCORD_USER_ID_RE.is_match(user_id)
    {
        errors.push(format!(
            "notify.discord.user_id: '{user_id}' is not a valid Discord user ID (must match ^[0-9]{{17,20}}$)"
        ));
    }
}

/// Validate the `announce:` block: the logical package and both repository
/// slugs must be well-formed `<a>/<b>` pairs, and the optional catch-up
/// schedule must be a cron expression safe to splice into a generated `on:`
/// block (see [`validate_cron`]).
///
/// A malformed value is reported as a named field error (contributing to
/// `SpecInvalid`, exit 65) rather than a serde shape mismatch, so the message
/// names the field and what it expected.
pub fn validate_announce_config(config: &AnnounceConfig, errors: &mut Vec<String>) {
    if let Some(cron) = &config.schedule {
        validate_cron("announce.schedule", cron, errors);
    }
    if !INDEX_PACKAGE_RE.is_match(&config.package) {
        errors.push(format!(
            "announce.package: '{}' is not a valid index package (must be '<namespace>/<package>', \
             lowercase alphanumeric with '.', '_' or '-')",
            config.package
        ));
    }
    for (field, value) in [("fork", &config.fork), ("index_repo", &config.index_repo)] {
        if !GITHUB_REPO_RE.is_match(value) {
            errors.push(format!(
                "announce.{field}: '{value}' is not a valid GitHub repository (must be '<owner>/<repo>')"
            ));
        }
    }
}
