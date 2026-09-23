// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-028 — the root rewrite is a pure byte-level transform over an
//! order-preserving `Value`: one key changes, everything else rides through.

use serde_json::{Value, json};

use super::super::*;
use super::support::*;

#[test]
fn rewrite_root_replaces_the_repository_pointer() {
    let source = root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &digest("a"))]);

    let rewritten = rewrite_root(&source, POINTER).expect("rewrite");

    let document: Value = serde_json::from_slice(&rewritten).expect("parse");
    assert_eq!(document["repository"], json!(POINTER));
}

#[test]
fn rewrite_root_carries_every_untouched_field_through_verbatim() {
    // A field no version of `IndexRoot` models. A typed round-trip would drop
    // it silently; the `Value` transform must not.
    let source = json!({
        "repository": "oci://ghcr.io/ocx-sh/cmake",
        "name": "cmake",
        "desc": "Cross-platform build system",
        "owners": ["ocx-sh"],
        "created": "2026-01-02T03:04:05Z",
        "upstream": "https://cmake.org",
        "status": "deprecated",
        "deprecated_message": "use ninja",
        "superseded_by": "tools/ninja",
        "a_field_from_a_newer_writer": { "nested": [1, 2, 3] },
        "tags": {},
    });
    let raw = serialize_root(&source);

    let rewritten = rewrite_root(&raw, POINTER).expect("rewrite");

    let document: Value = serde_json::from_slice(&rewritten).expect("parse");
    for field in [
        "name",
        "desc",
        "owners",
        "created",
        "upstream",
        "status",
        "deprecated_message",
        "superseded_by",
        "a_field_from_a_newer_writer",
    ] {
        assert_eq!(document[field], source[field], "{field} did not ride through verbatim");
    }
}

#[test]
fn rewrite_root_preserves_key_order() {
    // The guard on `serde_json`'s `preserve_order` feature. Without it
    // `serde_json::Map` is a `BTreeMap`, every root re-emits alphabetically,
    // and `sha256(root bytes)` changes for documents that did not semantically
    // change — with no compile error anywhere.
    let source = root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &digest("a"))]);
    let before = key_order(&source);

    let rewritten = rewrite_root(&source, POINTER).expect("rewrite");

    let mut alphabetical = before.clone();
    alphabetical.sort();
    assert_ne!(
        before, alphabetical,
        "the fixture must not already be in alphabetical order, or this test cannot fail"
    );
    assert_eq!(key_order(&rewritten), before);
}

#[test]
fn rewrite_root_emits_the_wire_form() {
    let source = json!({ "repository": "oci://ghcr.io/ocx-sh/cmake", "desc": "builds — fast", "tags": {} });

    let rewritten = rewrite_root(&serialize_root(&source), POINTER).expect("rewrite");

    let text = String::from_utf8(rewritten).expect("UTF-8");
    assert!(text.ends_with("}\n"), "one trailing newline: {text:?}");
    assert!(!text.ends_with("}\n\n"), "exactly one trailing newline: {text:?}");
    assert!(text.contains("\n  \"desc\""), "two-space indent: {text:?}");
    let escaped_em_dash = "\\u2014";
    assert!(text.contains(escaped_em_dash), "non-ASCII escaped as \\uXXXX: {text:?}");
    assert!(!text.contains('\u{2014}'), "no raw non-ASCII scalar survives: {text:?}");
    assert!(text.contains("\"tags\": {}"), "empty object inline: {text:?}");
}

#[test]
fn rewrite_root_refuses_a_document_that_is_not_an_object() {
    let error = rewrite_root(b"[]", POINTER).expect_err("an array is not a root document");

    assert!(refusal_message(&error).contains("not a JSON object"), "{error:?}");
}

#[test]
fn rewrite_root_refuses_bytes_that_are_not_json() {
    let error = rewrite_root(b"{oops", POINTER).expect_err("not JSON");

    assert!(refusal_message(&error).contains("not valid JSON"), "{error:?}");
}

#[test]
fn a_merge_then_rewrite_round_trip_is_byte_stable() {
    // The composition WP-14 performs. Running it twice over its own output
    // must be a fixed point, or every scheduled no-op run would churn mtimes
    // in a tree operators commit.
    let source: Value = serde_json::from_slice(&root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &digest("a")), ("latest", &digest("a"))],
    ))
    .expect("parse");
    let confirmed = confirmed(&["3.28.1", "latest"]);

    let merged = merge_root_tags(&source, None, &confirmed).expect("merge");
    let once = rewrite_root(&serialize_root(&merged), POINTER).expect("rewrite");

    let reparsed: Value = serde_json::from_slice(&once).expect("parse");
    let merged_again = merge_root_tags(&source, Some(&once), &confirmed).expect("merge");
    let twice = rewrite_root(&serialize_root(&merged_again), POINTER).expect("rewrite");

    assert_eq!(reparsed["repository"], json!(POINTER));
    assert_eq!(once, twice, "a second run over the same inputs changed the bytes");
}
