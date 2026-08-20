// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `RegistrySpec::validate` — C-006.
//!
//! The rejection corpus lives in `tests/fixtures/invalid_registry/`, one file
//! per rule. What is here is what a fixture loop cannot state: that a spec is
//! **valid**, that one added source flips a verdict on an otherwise unchanged
//! document, and that a spec breaking several rules reports all of them.

use super::support::*;

// ── Accept ──────────────────────────────────────────────────────────────────

#[test]
fn a_well_formed_spec_reports_nothing() {
    assert_eq!(validate(VALID_BODY), Vec::<String>::new());
}

/// `as:` is a path component, and a dot is legal in one — the whole reason
/// `localhost:5001` is refused while `ocx.sh` is not is the `:`, not the
/// punctuation in general (C-002).
#[test]
fn a_dotted_registry_name_is_a_legal_as_value() {
    for name in ["ocx.sh", "ghcr.io", "registry.internal.example.com", "a-b_c.d"] {
        let yaml = VALID_BODY.replace("as: upstream", &format!("as: {name}"));
        assert_eq!(validate(&yaml), Vec::<String>::new(), "`as: {name}` must be accepted");
    }
}

#[test]
fn a_source_may_carry_globs_the_engine_can_compile() {
    let yaml = r#"
target:
  registry: localhost:5002
  repository: mirror
output: public
destination: "{namespace}/{package}"
sources:
  - registry: localhost:5001
    index: https://index.example/
    as: upstream
    include:
      - "kitware/*"
      - "*"
    exclude:
      - "kitware/cmake-nightly"
"#;

    assert_eq!(validate(yaml), Vec::<String>::new());
}

// ── The transport rule, and the hatch that flips it ─────────────────────────

/// The refusal itself lives in `tests/fixtures/invalid_registry/`. What no
/// fixture can state is the **pairing**: the same plaintext URL is legal the
/// moment its host is listed, so the rule discriminates rather than refusing
/// every `http://` outright — which is what the acceptance harness and an
/// RFC1918 corporate index both depend on.
#[test]
fn a_plaintext_index_is_refused_and_a_trusted_hosts_entry_opens_it() {
    let plaintext = VALID_BODY.replace("index: https://index.example/", "index: http://localhost:5001/");

    let errors = validate(&plaintext);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("sources[0].index") && errors[0].contains("trusted_hosts"),
        "the message must name the field and the way out: {}",
        errors[0]
    );

    let opened = plaintext.replace(
        "    as: upstream\n",
        "    as: upstream\n    trusted_hosts: [\"localhost\"]\n",
    );
    assert_eq!(
        validate(&opened),
        Vec::<String>::new(),
        "a listed host is the same hatch the SSRF floor already honours"
    );
}

/// The hatch is judged on the **host**, not on the URL: an entry naming a
/// different host does not open this one.
#[test]
fn a_trusted_hosts_entry_for_another_host_does_not_open_a_plaintext_index() {
    let yaml = VALID_BODY
        .replace("index: https://index.example/", "index: http://index.example/")
        .replace(
            "    as: upstream\n",
            "    as: upstream\n    trusted_hosts: [\"other.example\"]\n",
        );

    let errors = validate(&yaml);

    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("index.example"), "{}", errors[0]);
}

/// An unparseable or hostless `index:` is `validate_index_base_host`'s
/// refusal, not this rule's — reporting one defect against two fields helps
/// nobody, and a second message would only bury the real one.
#[test]
fn an_unusable_index_url_is_left_to_the_pre_flight() {
    for unusable in ["not a url", "file:///srv/index"] {
        let yaml = VALID_BODY.replace("index: https://index.example/", &format!("index: \"{unusable}\""));
        assert_eq!(
            validate(&yaml),
            Vec::<String>::new(),
            "`{unusable}` must not be diagnosed here"
        );
    }
}

// ── The pairing no fixture can express (S-004) ──────────────────────────────

/// One source needs no `{registry}`; adding a second to the **same** document
/// makes it invalid.
///
/// A fixture can hold either half. Only a Rust test can assert that the
/// verdict flips on the added source rather than on anything else about the
/// two documents.
#[test]
fn a_second_source_is_what_makes_a_registry_less_destination_invalid() {
    let single = r#"
target:
  registry: localhost:5002
  repository: mirror
output: public
destination: "{namespace}/{package}"
sources:
  - registry: localhost:5001
    index: https://index.example/
    as: upstream
"#;
    let double = format!(
        "{single}\
         \n  - registry: localhost:5003\n    index: https://other.example/\n    as: other\n"
    );

    assert_eq!(
        validate(single),
        Vec::<String>::new(),
        "a single-source spec needs no {{registry}}"
    );

    let errors = validate(&double);
    assert_eq!(errors.len(), 1, "only the placeholder rule may fire: {errors:?}");
    assert!(
        errors[0].contains("{registry}"),
        "the message must name the placeholder to add: {}",
        errors[0]
    );
}

/// `{upstream_repository}` satisfies the multi-source rule that `{registry}`
/// otherwise carries.
///
/// The destination is then keyed by upstream identity: two sources meeting on
/// one repository named the same upstream package, so the second copy is a
/// duplicate rather than the overwrite the rule exists to prevent.
/// `{upstream_host}` alone does **not** qualify — two sources over one host
/// still collide on a shared catalog key — and that half is what makes this a
/// rule about the repository rather than about deferral.
#[test]
fn upstream_repository_answers_the_multi_source_rule_but_upstream_host_does_not() {
    let two_sources = |destination: &str| {
        format!(
            r#"
target:
  registry: localhost:5002
  repository: mirror
output: public
destination: "{destination}"
sources:
  - registry: localhost:5001
    index: https://index.example/
    as: upstream
  - registry: localhost:5003
    index: https://other.example/
    as: other
"#
        )
    };

    assert_eq!(
        validate(&two_sources("{upstream_host}/{upstream_repository}")),
        Vec::<String>::new(),
        "an upstream-keyed destination cannot have two sources overwrite one another"
    );

    let errors = validate(&two_sources("{upstream_host}/{namespace}/{package}"));
    assert_eq!(errors.len(), 1, "only the placeholder rule may fire: {errors:?}");
    assert!(
        errors[0].contains("{upstream_repository}"),
        "the message must offer the placeholder that would fix it: {}",
        errors[0]
    );
}

// ── Reporting shape ─────────────────────────────────────────────────────────

/// One run, one report: an operator fixing a spec should not have to re-run to
/// discover the next violation.
#[test]
fn every_violated_rule_is_reported_not_only_the_first() {
    let errors = validate(
        r#"
target:
  registry: localhost:5002
  repository: Mirror
output: ""
destination: "{namespace}/{package}"
concurrency:
  max_blobs: 0
  max_packages: 0
sources:
  - registry: localhost:5001
    index: https://index.example/
    as: upstream
    include:
      - "kitware/**"
  - registry: localhost:5003
    index: https://other.example/
    as: upstream
"#,
    );

    for needle in [
        "target.repository",
        "output",
        "{registry}",
        "max_blobs",
        "max_packages",
        "kitware/**",
        "already used by",
    ] {
        assert!(
            errors.iter().any(|error| error.contains(needle)),
            "no message mentions `{needle}`: {errors:?}"
        );
    }
}

/// The empty spec still reports the one thing an operator can act on, rather
/// than a cascade of consequences of having no sources.
#[test]
fn an_empty_sources_list_is_named_as_the_fault() {
    let errors = validate(
        r#"
target:
  registry: localhost:5002
  repository: mirror
output: public
destination: "{namespace}/{package}"
sources: []
"#,
    );

    assert_eq!(errors, vec!["sources: at least one source is required".to_string()]);
}

// ── The `as:` component rule, at the boundary the fallback creates ──────────

/// A source with no `as:` inherits the registry address verbatim, so a
/// registry carrying a port produces an `as:` no path can hold — and the
/// message has to name `as:` as the fix, because `registry:` is not wrong.
#[test]
fn a_ported_registry_address_used_as_the_fallback_is_refused_naming_as() {
    let yaml = VALID_BODY.replace("    as: upstream\n", "");
    let errors = validate(&yaml);

    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("as:") && errors[0].contains("localhost:5001"),
        "the message must name both the field to set and the value that failed: {}",
        errors[0]
    );
}

#[test]
fn an_as_value_spanning_two_path_components_is_refused() {
    let errors = validate(&VALID_BODY.replace("as: upstream", "as: ocx.sh/mirrors"));

    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("as:") && errors[0].contains("component"),
        "the message must say what is wrong with it: {}",
        errors[0]
    );
}

// ── The target prefix ───────────────────────────────────────────────────────

/// A registry carrying a path segment parses perfectly well — the segment just
/// silently migrates into the repository, re-homing every copied package one
/// level up from where the operator wrote it.
#[test]
fn a_registry_carrying_a_path_segment_is_refused() {
    let errors = validate(&VALID_BODY.replace("registry: localhost:5002", "registry: ghcr.io/ocx-contrib"));

    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("target.registry"),
        "the message must name the field at fault: {}",
        errors[0]
    );
}

/// A malformed `target.repository` is reported once, against the field that
/// owns it — the registry check parses the composed reference and would
/// otherwise blame `target.registry` for the same defect.
#[test]
fn a_malformed_repository_is_reported_once_against_its_own_field() {
    let errors = validate(&VALID_BODY.replace("repository: mirror", "repository: mirror/../prod"));

    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("target.repository"), "{}", errors[0]);
    assert!(
        !errors[0].contains("target.registry"),
        "the registry is not at fault here: {}",
        errors[0]
    );
}
