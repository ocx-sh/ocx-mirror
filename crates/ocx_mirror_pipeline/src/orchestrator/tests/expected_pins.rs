// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ExpectedMetadata` over a spec naming a dependency by tag alone (#90).
//!
//! Published metadata is always digest-pinned — only `ocx package create`
//! pins, per platform — so the download-free expectation cannot compute the
//! digest and has to adopt the one the registry records, the same way it
//! adopts a scanned `binaries` claim.

use super::super::*;
use super::support::*;

const TAG_ONLY: &str = "ocx.sh/adoptium/temurin:jre-25";
const PIN: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn authoring(identifier: &str) -> AuthoringMetadata {
    serde_json::from_str(&format!(
        r#"{{"type":"bundle","version":1,
            "dependencies":[{{"identifier":"{identifier}","name":"temurin","visibility":"private"}}]}}"#
    ))
    .expect("authoring fixture parses")
}

fn published(identifier: &str) -> Metadata {
    serde_json::from_str(&format!(
        r#"{{"type":"bundle","version":1,
            "dependencies":[{{"identifier":"{identifier}","name":"temurin","visibility":"private"}}]}}"#
    ))
    .expect("published fixture parses")
}

/// Rendering no longer refuses a tag-only dependency — that refusal is what
/// failed `pipeline plan` and `patch` on such a spec — but the expectation it
/// yields is incomplete, never a published document.
#[test]
fn a_tag_only_dependency_renders_an_incomplete_expectation() {
    let expected = ExpectedMetadata::render(authoring(TAG_ONLY), &platform("linux/amd64")).expect("renders");
    assert!(
        expected.published.is_none(),
        "an unpinned dependency has no published form"
    );
    assert_eq!(
        expected
            .unpinned_dependencies()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [TAG_ONLY],
    );
}

/// A tile published from this spec records the same identifier plus the
/// digest create resolved. Adopting it completes the expectation to exactly
/// what is published — no drift.
#[test]
fn a_published_pin_for_the_same_tag_is_adopted() {
    let platform = platform("linux/amd64");
    let recorded = published(&format!("{TAG_ONLY}@{PIN}"));
    let adopted = ExpectedMetadata::render(authoring(TAG_ONLY), &platform)
        .expect("renders")
        .adopting_pins_from(&recorded, &platform)
        .expect("adopts");

    let complete = adopted.published.expect("every dependency is pinned now");
    assert_eq!(
        serde_json::to_value(&complete).expect("serializes"),
        serde_json::to_value(&recorded).expect("serializes"),
        "the adopted expectation must be the published document",
    );
    assert!(
        adopted.sidecar_json.contains(PIN),
        "and the sidecar patch would push carries the pin",
    );
}

/// The spec moved the tag (or added the dependency): nothing published can
/// stand in for a pin only create can resolve, so the expectation stays
/// incomplete — drift that `patch` must refuse.
#[test]
fn a_changed_tag_adopts_nothing() {
    let platform = platform("linux/amd64");
    let recorded = published(&format!("ocx.sh/adoptium/temurin:jre-21@{PIN}"));
    let adopted = ExpectedMetadata::render(authoring(TAG_ONLY), &platform)
        .expect("renders")
        .adopting_pins_from(&recorded, &platform)
        .expect("adopts");
    assert!(
        adopted.published.is_none(),
        "a different tag's digest must not be adopted"
    );
    assert_eq!(adopted.unpinned_dependencies().len(), 1);
}

/// A digest the spec writes itself is the spec's claim, and is never replaced
/// by the published one — otherwise a hand-corrected pin could never drift.
#[test]
fn a_spec_pinned_dependency_is_left_alone() {
    let platform = platform("linux/amd64");
    let other = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let spec_pin = format!("{TAG_ONLY}@{other}");
    let adopted = ExpectedMetadata::render(authoring(&spec_pin), &platform)
        .expect("renders")
        .adopting_pins_from(&published(&format!("{TAG_ONLY}@{PIN}")), &platform)
        .expect("adopts");
    let complete = adopted.published.expect("pinned by the spec");
    assert_eq!(
        complete
            .dependencies()
            .iter()
            .next()
            .map(|dependency| dependency.identifier.to_string()),
        Some(spec_pin),
    );
}

/// A `dependencies` array of `(identifier, name)` entries.
fn dependencies(entries: &[(&str, &str)]) -> String {
    let entries: Vec<String> = entries
        .iter()
        .map(|(identifier, name)| format!(r#"{{"identifier":"{identifier}","name":"{name}","visibility":"private"}}"#))
        .collect();
    format!(
        r#"{{"type":"bundle","version":1,"dependencies":[{}]}}"#,
        entries.join(",")
    )
}

/// A spec pin under adoption: one tag-only dependency makes the expectation
/// incomplete, so adoption runs over the spec-pinned one beside it too — the
/// shape [`a_spec_pinned_dependency_is_left_alone`] never reaches. The spec's
/// digest must survive, and the document must read as drift. Two things hold
/// it: the `is_pinned` guard, and the match itself, since a pinned identifier
/// never equals a published one stripped of its digest.
#[test]
fn a_spec_pin_beside_a_tag_only_dependency_keeps_its_own_digest() {
    let platform = platform("linux/amd64");
    let spec_pin = format!("{TAG_ONLY}@sha256:{}", "2".repeat(64));
    let zlib_pin = format!("sha256:{}", "3".repeat(64));
    let spec: AuthoringMetadata = serde_json::from_str(&dependencies(&[
        (&spec_pin, "temurin"),
        ("ocx.sh/tools/zlib:1.3", "zlib"),
    ]))
    .expect("authoring fixture parses");
    let recorded: Metadata = serde_json::from_str(&dependencies(&[
        (&format!("{TAG_ONLY}@{PIN}"), "temurin"),
        (&format!("ocx.sh/tools/zlib:1.3@{zlib_pin}"), "zlib"),
    ]))
    .expect("published fixture parses");

    let adopted = ExpectedMetadata::render(spec, &platform)
        .expect("renders")
        .adopting_pins_from(&recorded, &platform)
        .expect("adopts");
    let complete = adopted
        .published
        .as_ref()
        .expect("the tag-only dependency adopted its pin");
    let identifiers: Vec<String> = complete
        .dependencies()
        .iter()
        .map(|dependency| dependency.identifier.to_string())
        .collect();
    assert_eq!(
        identifiers,
        [spec_pin, format!("ocx.sh/tools/zlib:1.3@{zlib_pin}")],
        "the spec's own pin is kept; only the tag-only dependency adopts",
    );
    assert_ne!(
        serde_json::to_value(complete).expect("serializes"),
        serde_json::to_value(&recorded).expect("serializes"),
        "a hand-edited pin must still read as drift",
    );
}

/// The registry is part of the match: the same repository and tag published
/// from another registry is a different package, and its digest pins nothing.
#[test]
fn a_pin_published_for_another_registry_is_not_adopted() {
    let platform = platform("linux/amd64");
    let recorded = published(&format!("mirror.example.com/adoptium/temurin:jre-25@{PIN}"));
    let adopted = ExpectedMetadata::render(authoring(TAG_ONLY), &platform)
        .expect("renders")
        .adopting_pins_from(&recorded, &platform)
        .expect("adopts");
    assert!(
        adopted.published.is_none(),
        "another registry's digest must not be adopted"
    );
}

/// Adoption takes the first published dependency matching registry,
/// repository and tag. That is unambiguous only because neither form lets one
/// repository appear twice — two tags of it cannot be spelled, so a second
/// candidate for the same match cannot exist.
#[test]
fn one_repository_cannot_be_named_under_two_tags() {
    let document = dependencies(&[(TAG_ONLY, "jre"), ("ocx.sh/adoptium/temurin:jdk-25", "jdk")]);
    serde_json::from_str::<AuthoringMetadata>(&document).expect_err("a spec cannot name one repository twice");
    let pinned = dependencies(&[
        (&format!("{TAG_ONLY}@{PIN}"), "jre"),
        (&format!("ocx.sh/adoptium/temurin:jdk-25@{PIN}"), "jdk"),
    ]);
    serde_json::from_str::<Metadata>(&pinned).expect_err("nor can a published document");
}

/// Rendering projects a tag-only document eagerly, through a placeholder pin
/// that must never reach either form: the sidecar is the file `patch` pushes,
/// and a published expectation carrying the placeholder would compare as
/// drift against every tile — or, adopted, publish a pin to nothing.
#[test]
fn the_eager_projection_leaves_no_placeholder_behind() {
    let platform = platform("linux/amd64");
    let spec_pin = format!("{TAG_ONLY}@{PIN}");
    let spec: AuthoringMetadata = serde_json::from_str(&dependencies(&[
        (&spec_pin, "temurin"),
        ("ocx.sh/tools/zlib:1.3", "zlib"),
    ]))
    .expect("authoring fixture parses");
    let expected = ExpectedMetadata::render(spec, &platform).expect("renders");

    assert!(
        expected.published.is_none(),
        "one tag-only dependency leaves it incomplete"
    );
    assert!(
        !expected.sidecar_json.contains(&"0".repeat(64)),
        "the placeholder must not reach the sidecar: {}",
        expected.sidecar_json,
    );
    assert!(
        expected.sidecar_json.contains(r#""ocx.sh/tools/zlib:1.3""#),
        "the tag-only identifier is written as the spec wrote it",
    );
    assert_eq!(
        expected
            .unpinned_dependencies()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["ocx.sh/tools/zlib:1.3"],
    );
}
