// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The `registry.yml` schema: C-001…C-004.
//!
//! What the *type* accepts, before any validation rule runs. A field that
//! deserializes when it should not is a defect no `validate()` rule can reach,
//! because `deny_unknown_fields` is the only thing standing between a typo and
//! a silently-ignored setting.

use super::super::*;
use super::support::*;

// ── C-001 — the root document ───────────────────────────────────────────────

#[test]
fn the_root_document_carries_target_output_destination_and_sources() {
    let spec = parse(VALID_BODY);

    assert_eq!(spec.target.registry, "localhost:5002");
    assert_eq!(spec.target.repository, "mirror");
    assert_eq!(spec.output, std::path::PathBuf::from("public"));
    assert_eq!(spec.destination, "{registry}/{namespace}/{package}");
    assert_eq!(spec.sources.len(), 1);
}

#[test]
fn absent_optional_blocks_take_their_documented_defaults() {
    let spec = parse(VALID_BODY);

    assert_eq!(
        spec.on_error,
        OnError::Continue,
        "a failure must not abort the run by default"
    );
    assert_eq!(spec.concurrency.max_blobs, 4);
    assert_eq!(spec.concurrency.max_retries, 3);
    assert!(
        !spec.rewrite_pointers,
        "a mirrored index must keep the upstream pointer unless the operator asks otherwise"
    );
}

/// `rewrite_pointers` is a plain bool, opted into by writing it.
///
/// Pinned separately from the defaults above because the two directions fail
/// differently: a wrong default silently re-homes an operator's whole fleet,
/// while a field that does not read at all silently ignores the operator who
/// wanted the old behaviour back.
#[test]
fn rewrite_pointers_is_read_when_the_document_sets_it() {
    let spec = parse(&format!("{VALID_BODY}\nrewrite_pointers: true\n"));

    assert!(spec.rewrite_pointers);
}

#[test]
fn an_unknown_top_level_key_is_a_serde_error() {
    let message = parse_error(&format!("{VALID_BODY}\nversions:\n  min: 1.0.0\n"));

    assert!(
        message.contains("versions"),
        "the message must name the unknown key: {message}"
    );
}

/// `kind:` is read by the pre-scan and by nothing else (C-001).
///
/// This is the test that makes C-007's `kind` strip load-bearing rather than
/// decorative: leave the key in the merged document and **every** valid
/// `registry.yml` fails here, before a single validation rule runs.
#[test]
fn kind_is_not_a_field_and_a_document_still_carrying_it_does_not_parse() {
    let message = parse_error(&valid_registry_yaml());

    assert!(
        message.contains("kind"),
        "the message must name the offending key: {message}"
    );
}

#[test]
fn an_empty_sources_list_parses_and_is_left_to_validate() {
    let spec = parse(
        r#"
target:
  registry: localhost:5002
  repository: mirror
output: public
destination: "{namespace}/{package}"
sources: []
"#,
    );

    assert!(spec.sources.is_empty());
}

// ── C-002 — one source ──────────────────────────────────────────────────────

#[test]
fn a_source_takes_empty_filter_lists_by_default() {
    let source = &parse(VALID_BODY).sources[0];

    assert_eq!(source.registry, "localhost:5001");
    assert_eq!(source.index, "https://index.example/");
    assert!(source.include.is_empty());
    assert!(source.exclude.is_empty());
    assert!(source.trusted_hosts.is_empty());
}

#[test]
fn as_is_read_from_the_yaml_keyword_not_the_field_name() {
    let spec = parse(VALID_BODY);
    assert_eq!(spec.sources[0].as_name(), "upstream");

    let message = parse_error(&VALID_BODY.replace("as: upstream", "as_name: upstream"));
    assert!(
        message.contains("as_name"),
        "`as_name:` is not the spelling; the message must say so: {message}"
    );
}

/// The fallback is **verbatim**, never slugified: the value is a served path
/// segment operators point `[registries]` at, so it has to read back exactly as
/// written — and when the registry address cannot serve as one, that is a
/// validation error naming `as:`, not a silent repair.
#[test]
fn as_name_falls_back_to_the_registry_address_verbatim() {
    for registry in ["ocx.sh", "ghcr.io", "localhost:5001", "registry.internal.example.com"] {
        let spec = parse(
            &VALID_BODY
                .replace("    as: upstream\n", "")
                .replace("  - registry: localhost:5001", &format!("  - registry: {registry}")),
        );

        assert_eq!(
            spec.sources[0].as_name(),
            registry,
            "the fallback must not transform the value in any way"
        );
    }
}

#[test]
fn an_unknown_key_inside_a_source_is_a_serde_error() {
    let message = parse_error(&VALID_BODY.replace("    as: upstream", "    trusted_host: example.com"));

    assert!(
        message.contains("trusted_host"),
        "the near-miss singular must be named: {message}"
    );
}

// ── C-003 — concurrency ─────────────────────────────────────────────────────

#[test]
fn concurrency_defaults_each_knob_independently() {
    let spec = parse(&format!("{VALID_BODY}\nconcurrency:\n  max_blobs: 8\n"));

    assert_eq!(spec.concurrency.max_blobs, 8);
    assert_eq!(spec.concurrency.max_retries, 3, "the unset knob keeps its default");
}

#[test]
fn an_unknown_key_inside_concurrency_is_a_serde_error() {
    let message = parse_error(&format!("{VALID_BODY}\nconcurrency:\n  max_downloads: 8\n"));

    assert!(
        message.contains("max_downloads"),
        "`ConcurrencyConfig`'s knob is not this type's knob, and the message must say so: {message}"
    );
}

// ── C-004 — the failure policy ──────────────────────────────────────────────

#[test]
fn on_error_spells_its_variants_in_snake_case() {
    for (written, expected) in [("continue", OnError::Continue), ("fail_fast", OnError::FailFast)] {
        let spec = parse(&format!("{VALID_BODY}\non_error: {written}\n"));
        assert_eq!(spec.on_error, expected);
    }
}

#[test]
fn an_unrecognised_on_error_value_names_both_legal_spellings() {
    let message = parse_error(&format!("{VALID_BODY}\non_error: fail-fast\n"));

    for spelling in ["continue", "fail_fast"] {
        assert!(
            message.contains(spelling),
            "the message must name `{spelling}` so the fix is readable off it: {message}"
        );
    }
}

#[test]
fn the_default_failure_policy_is_continue() {
    assert_eq!(OnError::default(), OnError::Continue);
}
