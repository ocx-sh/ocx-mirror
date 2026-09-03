// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Test corpus for `sign_config.rs` — C-050 (shape) and C-051 (refusals).
//!
//! Two layers, deliberately kept apart because they fail differently:
//!
//! * **C-050** is serde's job — the `sign:` grammar, the [`Ref`] spellings and
//!   the two `key:` forms. Every case here exercises shipped code.
//! * **C-051** is [`validate_sign_config`]'s — the refusals that need to name
//!   a *field*, which the value grammar alone cannot express (see [`Ref`]'s
//!   own doc comment for why the split exists).
//!
//! Where a case is only expressible as a document (an unknown key, a null
//! `sign:`, a `key:` map missing `ref`) it goes through
//! [`load_spec`](crate::spec::load_spec), because the exit code C-050/C-051
//! promise is a property of that path and not of the serde error alone.

use std::path::Path;

use ocx_lib::cli::ExitCode;

use super::*;
use crate::error::MirrorError;
use crate::spec::{MirrorSpec, load_spec, validate_sign_config};

/// A value distinctive enough that finding it anywhere in a message is proof
/// of a leak rather than a coincidence — the C-051 "never echoes the value"
/// half is what most of this file's refusal assertions are really pinning.
const SENTINEL: &str = "hunter2-canary";

/// The four fixtures WP 1 ships, paired with the `sign:` shape each names.
const SIGN_FIXTURES: &[&str] = &[
    "mirror-sign-keyless.yml",
    "mirror-sign-keyless-endpoints.yml",
    "mirror-sign-key.yml",
    "mirror-sign-key-full.yml",
];

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
}

/// Deserialize a `sign:` body (the block's *contents*, without the key).
fn sign(yaml: &str) -> SignConfig {
    serde_yaml_ng::from_str(yaml).unwrap_or_else(|e| panic!("`sign:` body must parse: {e}\n{yaml}"))
}

/// Run [`validate_sign_config`] over a `sign:` body and return its refusal.
fn refusal(yaml: &str) -> MirrorError {
    validate_sign_config(&sign(yaml)).expect_err("the `sign:` block must be refused")
}

/// A complete, otherwise-valid `mirror.yml` carrying `sign_block` verbatim.
fn mirror_yaml(sign_block: &str) -> String {
    format!(
        r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
platforms:
  linux/amd64:
    runner: ubuntu-latest
{sign_block}
"#
    )
}

/// Load a `mirror.yml` carrying `sign_block` through the real loader.
///
/// The temporary directory is bound to a `let` for the whole call: dropping
/// it inline would delete the spec before `load_spec` opened it (TEST-06).
async fn load(sign_block: &str) -> Result<MirrorSpec, MirrorError> {
    let dir = tempfile::tempdir().expect("temporary directory");
    let spec_path = dir.path().join("mirror.yml");
    std::fs::write(&spec_path, mirror_yaml(sign_block)).expect("spec is writable");
    load_spec(&spec_path).await
}

/// Load a `mirror.yml` carrying `sign_block` and return its rejection.
async fn load_rejection(sign_block: &str) -> MirrorError {
    load(sign_block)
        .await
        .err()
        .unwrap_or_else(|| panic!("the document must be rejected:\n{sign_block}"))
}

/// A `!tag` on `sign:` must not smuggle a raw shape past the refusals.
///
/// This guard needs no `Value::Tagged` arm, unlike the credential scan in
/// `prescan.rs`, and the difference is not an oversight: `prescan` matches
/// variants explicitly, so a `Tagged` slips past `Value::Mapping(_)`, whereas
/// `as_mapping()` sees **through** a tag and returns the inner mapping. This
/// test pins that, because the two guards reading the same document by
/// different mechanisms is exactly the asymmetry a future rewrite would get
/// wrong — swap these `as_mapping()` calls for a `match` on `Value` and a
/// `!tag` silently skips every refusal below.
///
/// Calls the guard **directly**, not through `load_spec`: routed through the
/// loader, `pre_scan` and the typed seat reject these documents for their own
/// reasons, so the assertion stays green even with this guard disabled — it
/// would prove nothing. Asserting on the guard's own message is what makes it
/// load-bearing.
#[test]
fn a_tag_on_sign_does_not_hide_a_raw_shape() {
    for (yaml, needle) in [
        (
            "sign: !anything\n  key:\n    ref: env://K\n    passphrase: 12345\n",
            "must be a quoted string reference",
        ),
        (
            "sign: !anything\n  keyless:\n",
            "is null; give it a value or remove the key",
        ),
        ("sign: !anything\n  key:\n    passphrase: env://P\n", "sign.key.ref"),
    ] {
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).expect("the fixture parses");
        let error = crate::spec::validate::refuse_raw_sign_shapes(&value, std::path::Path::new("mirror.yml"))
            .expect_err(&format!("a tagged sign: block must still be refused:\n{yaml}"));
        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "{yaml}");
        let message = error.to_string();
        assert!(message.contains(needle), "the guard's own refusal must fire: {message}");
    }
}

// ── C-050 — the shape ───────────────────────────────────────────────────

/// `keyless: {}` is the whole public-Sigstore configuration: a tag with no
/// endpoints, which WP 2 fills from the mirror-owned defaults.
#[test]
fn an_empty_keyless_map_selects_the_keyless_tag() {
    assert_eq!(
        sign("keyless: {}\n"),
        SignConfig {
            keyless: Some(KeylessConfig {
                fulcio: None,
                rekor: None,
                identity_token: None,
            }),
            key: None,
        }
    );
}

#[test]
fn keyless_carries_its_three_optional_refs() {
    assert_eq!(
        sign(
            "keyless:\n  fulcio: env://SIGSTORE_FULCIO_URL\n  rekor: https://rekor.example\n  identity_token: file:///run/token\n"
        ),
        SignConfig {
            keyless: Some(KeylessConfig {
                fulcio: Some(Ref::Env("SIGSTORE_FULCIO_URL".to_string())),
                rekor: Some(Ref::Literal("https://rekor.example".to_string())),
                identity_token: Some(Ref::File(PathBuf::from("/run/token"))),
            }),
            key: None,
        }
    );
}

/// The string form is shorthand for `{ ref: <ref> }` — no passphrase, no
/// Rekor upload — so it must reach `KeyConfig::Reference`, not a `Full` with
/// defaulted fields, or WP 2 cannot tell "no rekor named" from "rekor named".
#[test]
fn key_in_string_form_is_a_bare_reference() {
    assert_eq!(
        sign("key: env://MIRROR_SIGNING_KEY\n"),
        SignConfig {
            keyless: None,
            key: Some(KeyConfig::Reference(Ref::Env("MIRROR_SIGNING_KEY".to_string()))),
        }
    );
    // A literal (a bare path, as ocx takes it) is the same form.
    assert_eq!(
        sign("key: ./cosign.key\n"),
        SignConfig {
            keyless: None,
            key: Some(KeyConfig::Reference(Ref::Literal("./cosign.key".to_string()))),
        }
    );
}

#[test]
fn key_in_map_form_carries_ref_passphrase_and_rekor() {
    assert_eq!(
        sign(
            "key:\n  ref: file:///run/secrets/mirror.key\n  passphrase: env://MIRROR_KEY_PASSPHRASE\n  rekor: env://SIGSTORE_REKOR_URL\n"
        ),
        SignConfig {
            keyless: None,
            key: Some(KeyConfig::Full(KeyFullConfig {
                reference: Ref::File(PathBuf::from("/run/secrets/mirror.key")),
                passphrase: Some(Ref::Env("MIRROR_KEY_PASSPHRASE".to_string())),
                rekor: Some(Ref::Env("SIGSTORE_REKOR_URL".to_string())),
            })),
        }
    );
    // Only `ref` is required; the other two are absent, not empty.
    assert_eq!(
        sign("key:\n  ref: env://K\n"),
        SignConfig {
            keyless: None,
            key: Some(KeyConfig::Full(KeyFullConfig {
                reference: Ref::Env("K".to_string()),
                passphrase: None,
                rekor: None,
            })),
        }
    );
}

#[test]
fn an_absent_sign_block_is_none() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(&mirror_yaml("")).expect("a spec without `sign:` must parse");
    assert!(spec.sign.is_none());
}

/// Every `Ref` spelling, including the ones a later C-051 refusal rejects:
/// the grammar is a split on a prefix and **never** a rejection, so an empty
/// `NAME`/`PATH` and a malformed variable name both parse here and are
/// refused by `validate_sign_config` instead, where the field can be named.
#[test]
fn every_ref_spelling_parses_and_none_is_rejected() {
    let cases: &[(&str, Ref)] = &[
        ("literal", Ref::Literal("literal".to_string())),
        (
            "https://fulcio.sigstore.dev",
            Ref::Literal("https://fulcio.sigstore.dev".to_string()),
        ),
        ("", Ref::Literal(String::new())),
        ("env://NAME", Ref::Env("NAME".to_string())),
        // Refused later by C-051, but parsed here.
        ("env://lower", Ref::Env("lower".to_string())),
        ("env://", Ref::Env(String::new())),
        ("file:///abs/path", Ref::File(PathBuf::from("/abs/path"))),
        ("file://relative", Ref::File(PathBuf::from("relative"))),
        ("file://", Ref::File(PathBuf::new())),
        // A prefix that is not one of the two is a literal, not an error.
        ("kms://alias/x", Ref::Literal("kms://alias/x".to_string())),
    ];

    for (spelling, expected) in cases {
        assert_eq!(
            &Ref::from((*spelling).to_string()),
            expected,
            "spelling {spelling:?} parsed to the wrong kind"
        );
        // The same value through serde, since that is how a spec reaches it.
        let parsed: Ref = serde_yaml_ng::from_str(&format!("{spelling:?}"))
            .unwrap_or_else(|e| panic!("{spelling:?} must deserialize as a Ref: {e}"));
        assert_eq!(&parsed, expected, "spelling {spelling:?} differed through serde");
    }
}

/// `String -> Ref -> String` is the round-trip WP 2 depends on: what it hands
/// to ocx's `--key`/`--rekor-url` is the string form of what the spec said.
#[test]
fn a_ref_round_trips_through_its_string_form() {
    for spelling in [
        "literal",
        "https://rekor.sigstore.dev",
        "",
        "env://NAME",
        "env://",
        "file:///abs/path",
        "file://",
        "kms://alias/x",
    ] {
        let round_tripped = String::from(Ref::from(spelling.to_string()));
        assert_eq!(round_tripped, spelling, "{spelling:?} did not survive the round trip");
    }

    // ...and from the value side, so a `Ref` built in code renders the same
    // spelling a spec would have written.
    for value in [
        Ref::Literal("plain".to_string()),
        Ref::Env("NAME".to_string()),
        Ref::File(PathBuf::from("/abs/path")),
    ] {
        assert_eq!(
            Ref::from(String::from(value.clone())),
            value,
            "{value:?} did not survive the round trip"
        );
    }
}

#[test]
fn a_ref_round_trips_through_yaml() {
    for value in [
        Ref::Literal("plain".to_string()),
        Ref::Env("NAME".to_string()),
        Ref::File(PathBuf::from("/abs/path")),
    ] {
        let rendered = serde_yaml_ng::to_string(&value).expect("a Ref serializes");
        let parsed: Ref = serde_yaml_ng::from_str(&rendered).expect("a rendered Ref parses");
        assert_eq!(parsed, value, "{value:?} did not survive YAML");
    }
}

/// An unknown key is a typo that would otherwise publish under a
/// configuration the operator did not write — and the message has to name the
/// key, which is the whole reason `key:` is hand-deserialized rather than
/// `#[serde(untagged)]`.
#[tokio::test]
async fn an_unknown_key_is_refused_by_name_at_every_level() {
    let cases: &[(&str, &str)] = &[
        ("sign:\n  keyless: {}\n  mode: keyless\n", "mode"),
        ("sign:\n  keyless:\n    fulcio_url: https://f.example\n", "fulcio_url"),
        ("sign:\n  key:\n    ref: env://K\n    passphrse: env://P\n", "passphrse"),
    ];

    for (block, field) in cases {
        let error = load_rejection(block).await;
        assert_eq!(
            error.kind_exit_code(),
            ExitCode::DataError,
            "an unknown field is malformed data (65): {error}"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains(&format!("unknown field `{field}`")),
            "the message must name the unknown field: {rendered}"
        );
    }
}

/// A `key:` that is neither a string nor a map must say what both accepted
/// shapes are — an untagged enum would say only "did not match any variant".
#[tokio::test]
async fn a_key_that_is_neither_a_string_nor_a_map_names_both_shapes() {
    let error = load_rejection("sign:\n  key:\n    - env://K\n").await;
    let rendered = error.to_string();

    assert!(
        rendered.contains("a string `ref`, or a map with `ref` and optional `passphrase`/`rekor`"),
        "the message must name both accepted shapes: {rendered}"
    );
}

/// The four shipped fixtures are the spec surface a reader copies from; if
/// one stops deserializing to the shape its comment claims, the documentation
/// is wrong in the same commit.
#[test]
fn the_sign_fixtures_deserialize_to_the_shapes_they_name() {
    let expected: &[SignConfig] = &[
        SignConfig {
            keyless: Some(KeylessConfig {
                fulcio: None,
                rekor: None,
                identity_token: None,
            }),
            key: None,
        },
        SignConfig {
            keyless: Some(KeylessConfig {
                fulcio: Some(Ref::Env("SIGSTORE_FULCIO_URL".to_string())),
                rekor: Some(Ref::Env("SIGSTORE_REKOR_URL".to_string())),
                identity_token: Some(Ref::File(PathBuf::from("/run/secrets/sigstore-id-token"))),
            }),
            key: None,
        },
        SignConfig {
            keyless: None,
            key: Some(KeyConfig::Reference(Ref::Env("MIRROR_SIGNING_KEY".to_string()))),
        },
        SignConfig {
            keyless: None,
            key: Some(KeyConfig::Full(KeyFullConfig {
                reference: Ref::File(PathBuf::from("/run/secrets/mirror.key")),
                passphrase: Some(Ref::Env("MIRROR_KEY_PASSPHRASE".to_string())),
                rekor: Some(Ref::Env("SIGSTORE_REKOR_URL".to_string())),
            })),
        },
    ];

    for (name, want) in SIGN_FIXTURES.iter().zip(expected) {
        let source = std::fs::read_to_string(fixture_path(name)).unwrap_or_else(|e| panic!("{name} is readable: {e}"));
        let spec: MirrorSpec = serde_yaml_ng::from_str(&source).unwrap_or_else(|e| panic!("{name} must parse: {e}"));
        assert_eq!(spec.sign.as_ref(), Some(want), "{name} carries the wrong `sign:` shape");
    }
}

/// End to end: each fixture survives the whole loader, so `sign:` costs an
/// operator nothing beyond the block itself.
#[tokio::test]
async fn the_sign_fixtures_load_through_load_spec() {
    for name in SIGN_FIXTURES {
        let path = fixture_path(name);
        let spec = load_spec(&path)
            .await
            .unwrap_or_else(|e| panic!("{name} must load: {e}"));
        assert!(spec.sign.is_some(), "{name} lost its `sign:` block");
    }
}

// ── C-051 — the refusals ────────────────────────────────────────────────

/// A `sign:` block naming neither mode is the S-051 hazard in its second
/// spelling: honoured as written it would publish unsigned while the spec
/// says signing is configured.
#[test]
fn a_sign_block_with_neither_tag_is_refused() {
    let error = refusal("{}\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        error.to_string().contains("sign"),
        "the message must name the block: {error}"
    );
}

#[test]
fn both_mode_tags_together_are_refused() {
    let error = refusal("keyless: {}\nkey: env://MIRROR_SIGNING_KEY\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    let rendered = error.to_string();
    // The backticked forms, because a bare `contains("key")` is satisfied by
    // the `keyless` the message already carries — the weak spelling passes a
    // message that dropped one tag entirely.
    assert!(
        rendered.contains("`keyless:`") && rendered.contains("`key:`"),
        "the message must name both tags distinctly: {rendered}"
    );
}

/// The string form is `sign.key` and the map form `sign.key.ref` (C-051), so
/// each label is pinned separately: asserting the common `sign.key` prefix
/// passes even if the two are swapped, which points the operator at a line
/// they did not write. The trailing colon is what makes `sign.key:` and
/// `sign.key.ref:` distinguishable at all.
#[test]
fn an_empty_ref_is_refused() {
    for (block, field) in [("key: \"\"\n", "sign.key:"), ("key:\n  ref: \"\"\n", "sign.key.ref:")] {
        let error = refusal(block);
        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "not refused: {block}");
        assert!(
            error.to_string().contains(field),
            "the message must name {field}: {error}"
        );
    }
}

/// A PEM body pasted where a *reference* belongs is private key material in a
/// file that gets committed. Refused, and — the assertion that matters — the
/// message must not quote it back into every log the run writes.
#[test]
fn a_pem_body_as_a_ref_is_refused_without_echoing_it() {
    let error = refusal(&format!(
        "key: \"-----BEGIN PRIVATE KEY-----\\n{SENTINEL}\\n-----END PRIVATE KEY-----\"\n"
    ));
    let rendered = error.to_string();

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        !rendered.contains(SENTINEL),
        "the key material leaked into the message: {rendered}"
    );
    assert!(
        rendered.contains("sign.key:"),
        "the message must name the field: {rendered}"
    );
}

#[test]
fn a_literal_passphrase_is_refused_without_echoing_it() {
    let error = refusal(&format!("key:\n  ref: env://K\n  passphrase: {SENTINEL}\n"));
    let rendered = error.to_string();

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        !rendered.contains(SENTINEL),
        "the passphrase leaked into the message: {rendered}"
    );
    assert!(
        rendered.contains("sign.key.passphrase:"),
        "the message must name the dotted field: {rendered}"
    );
}

#[test]
fn a_literal_identity_token_is_refused_without_echoing_it() {
    let error = refusal(&format!("keyless:\n  identity_token: {SENTINEL}\n"));
    let rendered = error.to_string();

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        !rendered.contains(SENTINEL),
        "the identity token leaked into the message: {rendered}"
    );
    assert!(
        rendered.contains("sign.keyless.identity_token:"),
        "the message must name the dotted field: {rendered}"
    );
}

/// A variable name outside `^[A-Z_][A-Z0-9_]*$` is one an operator cannot
/// export portably; refusing it at the spec is the only place the *field* is
/// still known.
#[test]
fn an_env_ref_with_a_malformed_variable_name_is_refused() {
    for name in ["lower", "1ABC", "A-B", ""] {
        let error = refusal(&format!("key: env://{name}\n"));
        assert_eq!(
            error.kind_exit_code(),
            ExitCode::UsageError,
            "env://{name} was not refused"
        );
        assert!(
            error.to_string().contains("sign.key:"),
            "the message must name the field for env://{name}: {error}"
        );
    }
}

/// The inverse, so the check cannot be satisfied by refusing everything.
///
/// `OCX_SIGNING_KEY` is deliberately absent: it is well-formed *and* refused,
/// by the separate scrub rule below.
#[test]
fn an_env_ref_with_a_well_formed_variable_name_is_accepted() {
    for name in ["A", "_A", "MIRROR_SIGNING_KEY", "K9", "_"] {
        assert!(
            validate_sign_config(&sign(&format!("key: env://{name}\n"))).is_ok(),
            "env://{name} must be accepted"
        );
    }
}

/// Every `Ref` seat under `sign:` refuses an `env://` name that ocx's plugin
/// dispatch scrubs.
///
/// Under `ocx mirror package pipeline push` those three variables are empty in
/// the mirror's environment and in the `ocx` child's, so the reference can
/// never resolve; run directly, it resolves fine. That inconsistency is the
/// whole rule, and it is all four seats have in common — `key`/`key.ref` reach
/// the child verbatim and fail after the cascade is tagged, `passphrase`/
/// `identity_token` are resolved by the mirror and fail before the first push.
#[test]
fn an_env_ref_naming_a_dispatch_scrubbed_variable_is_refused() {
    for name in ocx_lib::env::keys::CREDENTIAL_KEYS {
        for (field, block) in [
            ("sign.key", format!("key: env://{name}\n")),
            ("sign.key.ref", format!("key:\n  ref: env://{name}\n")),
            (
                "sign.key.passphrase",
                format!("key:\n  ref: file:///run/secrets/mirror.key\n  passphrase: env://{name}\n"),
            ),
            (
                "sign.keyless.identity_token",
                format!("keyless:\n  identity_token: env://{name}\n"),
            ),
        ] {
            let error = refusal(&block);

            assert_eq!(
                error.kind_exit_code(),
                ExitCode::UsageError,
                "env://{name} under {field} must be exit 64"
            );
            let message = error.to_string();
            assert!(
                message.contains(field) && message.contains(name),
                "the message must name the field and the variable: {message}"
            );
            // The refusal is about inconsistency, not readability: a direct
            // run reads these fine. Assert the message says what actually
            // goes wrong, or it will drift back to the false claim.
            assert!(
                message.contains("reserved by ocx") && message.contains("plugin dispatch"),
                "the message must say the name is ocx's and name the mechanism: {message}"
            );
            assert!(
                message.contains("resolve to nothing under `ocx mirror ...`"),
                "the message must name the one outcome common to every seat: {message}"
            );
            // Paired with the positive above so a rewrite that drops the
            // mechanism cannot pass on this alone. `publish unsigned` is what
            // happens to `key`/`key.ref`, whose ref reaches the ocx child
            // verbatim; `passphrase`/`identity_token` are resolved by the
            // mirror before the first push and publish nothing at all. A
            // message claiming either for all four seats is wrong twice over.
            assert!(
                !message.contains("publish unsigned"),
                "a consequence that holds for two of the four seats must not be claimed for all: {message}"
            );
        }
    }
}

#[test]
fn an_empty_file_path_is_refused() {
    let error = refusal("key: file://\n");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        error.to_string().contains("sign.key:"),
        "the message must name the field: {error}"
    );
}

/// Every shape the shipped fixtures teach must survive validation, or the
/// refusals above are over-broad and the documentation is a trap.
#[test]
fn every_documented_sign_shape_is_accepted() {
    for block in [
        "keyless: {}\n",
        "keyless:\n  fulcio: env://SIGSTORE_FULCIO_URL\n  rekor: env://SIGSTORE_REKOR_URL\n  identity_token: file:///run/secrets/sigstore-id-token\n",
        "key: env://MIRROR_SIGNING_KEY\n",
        "key: ./cosign.key\n",
        // A KMS reference is a literal to us — ocx resolves the scheme, and
        // refusing unknown schemes here would break every non-file key.
        "key: awskms:///arn:aws:kms:eu-central-1:111122223333:key/abcd\n",
        "key:\n  ref: file:///run/secrets/mirror.key\n  passphrase: env://MIRROR_KEY_PASSPHRASE\n  rekor: env://SIGSTORE_REKOR_URL\n",
    ] {
        assert!(
            validate_sign_config(&sign(block)).is_ok(),
            "a documented shape was refused:\n{block}"
        );
    }
}

/// `key: {}` names nothing — ocx has no default signing key — and a `key:`
/// map missing `ref` is the same document one typo later. Both are C-051
/// usage errors (64) naming `sign.key.ref`, not shape errors: the operator
/// wrote a `sign:` block that cannot be honoured, which is a different
/// failure from a malformed document.
#[tokio::test]
async fn a_key_map_without_a_ref_is_refused_as_a_usage_error() {
    for block in ["sign:\n  key: {}\n", "sign:\n  key:\n    passphrase: env://P\n"] {
        let error = load_rejection(block).await;
        assert_eq!(
            error.kind_exit_code(),
            ExitCode::UsageError,
            "not a usage error: {error}\n{block}"
        );
        assert!(
            error.to_string().contains("sign.key.ref"),
            "the message must name the missing field: {error}"
        );
    }
}

/// `sign:` with a null value deserializes `Option<SignConfig>` to `None` —
/// indistinguishable from an absent key — so the mirror would publish
/// unsigned while the spec says otherwise (the S-051 hazard). Refused on the
/// raw document, before deserialization, which is the only place the
/// distinction still exists.
#[tokio::test]
async fn a_null_sign_block_is_refused_naming_sign() {
    let error = load_rejection("sign:\n").await;

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        error.to_string().contains("sign"),
        "the message must name the block: {error}"
    );
}

// ── C-051 — the raw seat, and that the typed seat is wired ──────────────

/// **The wiring assertion.** Every other C-051 case above calls
/// [`validate_sign_config`] directly, so deleting its call site in
/// `load_spec` leaves the whole suite green while a literal secret ships.
/// This one goes through the loader and would go red for that deletion
/// alone.
#[tokio::test]
async fn a_literal_secret_is_refused_through_the_loader() {
    let error = load_rejection(&format!(
        "sign:\n  key:\n    ref: env://K\n    passphrase: {SENTINEL}\n"
    ))
    .await;
    let rendered = error.to_string();

    assert_eq!(
        error.kind_exit_code(),
        ExitCode::UsageError,
        "not a usage error: {error}"
    );
    assert!(
        rendered.contains("sign.key.passphrase:"),
        "the message must name the dotted field: {rendered}"
    );
    assert!(
        !rendered.contains(SENTINEL),
        "the passphrase leaked through the loader: {rendered}"
    );
}

/// A null mode tag erases itself exactly as a null `sign:` does, one level
/// down: `keyless:` with no value deserializes to `None`, so the document
/// reads as plain key mode — or, alone, as naming no mode at all, which
/// reports "neither tag" against a spec that visibly names one.
#[tokio::test]
async fn a_null_mode_tag_is_refused_naming_that_tag() {
    let cases: &[(&str, &str)] = &[
        ("sign:\n  keyless:\n  key: env://K\n", "sign.keyless:"),
        ("sign:\n  keyless: {}\n  key:\n", "sign.key:"),
        ("sign:\n  keyless:\n", "sign.keyless:"),
    ];

    for (block, field) in cases {
        let error = load_rejection(block).await;
        assert_eq!(
            error.kind_exit_code(),
            ExitCode::UsageError,
            "not a usage error: {error}\n{block}"
        );
        assert!(
            error.to_string().contains(field),
            "the message must name {field}: {error}\n{block}"
        );
    }
}

/// An unquoted scalar in a secret-class field is the one shape where serde's
/// own type error quotes the value back — `invalid type: integer \`1234567\``
/// — putting a passphrase into every log the run writes. Refused before
/// deserialization, so the digits never reach a message.
#[tokio::test]
async fn a_non_string_secret_is_refused_without_echoing_the_scalar() {
    let cases: &[(&str, &str)] = &[
        (
            "sign:\n  key:\n    ref: env://K\n    passphrase: 1234567\n",
            "sign.key.passphrase:",
        ),
        (
            "sign:\n  keyless:\n    identity_token: 1234567\n",
            "sign.keyless.identity_token:",
        ),
    ];

    for (block, field) in cases {
        let error = load_rejection(block).await;
        let rendered = error.to_string();
        assert_eq!(
            error.kind_exit_code(),
            ExitCode::UsageError,
            "not a usage error: {error}\n{block}"
        );
        assert!(rendered.contains(field), "the message must name {field}: {rendered}");
        assert!(
            !rendered.contains("1234567"),
            "the scalar leaked into the message: {rendered}"
        );
    }
}

/// Pins raw-before-typed. Both tags are set *and* the `key:` map is empty;
/// C-051's listed order would report the mutual exclusion first, but the raw
/// seat runs before deserialization and so gets the block. Swapping the two
/// seats turns this red.
#[tokio::test]
async fn a_raw_shape_is_refused_before_a_typed_one() {
    let error = load_rejection("sign:\n  keyless: {}\n  key: {}\n").await;

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError);
    assert!(
        error.to_string().contains("sign.key.ref:"),
        "the raw seat must win: {error}"
    );
}
