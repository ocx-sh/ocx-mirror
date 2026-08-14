// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-047 — the merge that makes root writes **additive**.
//!
//! `CatalogTransaction::write_root` is merge-blind, so without this the first
//! `on_error: continue` run with a failed content copy would write a root
//! holding only the tags that succeeded and silently delete the rest.

use serde_json::{Value, json};

use super::super::*;
use super::support::*;

/// The source root as a `Value`, which is what the merge takes.
fn source_value(tags: &[(&str, &Digest)]) -> Value {
    serde_json::from_slice(&root_document("oci://ghcr.io/ocx-sh/cmake", tags)).expect("parse")
}

#[test]
fn a_tag_this_run_did_not_confirm_survives_with_its_original_digest() {
    // The headline regression. Every all-succeed test passes without the merge.
    //
    // Run 1 mirrored {3.28.1, latest}. Run 2's source moved both on, but only
    // `3.28.1`'s content copied — `latest` failed and is NOT confirmed.
    let run_one = digest("run-one");
    let run_two = digest("run-two");
    let destination = root_document(POINTER, &[("3.28.1", &run_one), ("latest", &run_one)]);
    let source = source_value(&[("3.28.1", &run_two), ("latest", &run_two)]);
    let confirmed = confirmed(&["3.28.1"]);

    // RED — the pre-C-047 behaviour, spelled as the input that produces it:
    // ignore the destination and write the confirmed set alone.
    let without_destination = merge_root_tags(&source, None, &confirmed).expect("merge");
    assert_eq!(
        tag_digests(&serialize_root(&without_destination)),
        vec![("3.28.1".to_string(), run_two.to_string())],
        "confirmed-only is exactly the deletion this contract exists to prevent"
    );

    // GREEN — with the destination in hand, `latest` survives untouched.
    let merged = merge_root_tags(&source, Some(&destination), &confirmed).expect("merge");
    assert_eq!(
        tag_digests(&serialize_root(&merged)),
        vec![
            ("3.28.1".to_string(), run_two.to_string()),
            ("latest".to_string(), run_one.to_string()),
        ],
        "`latest` must survive at its ORIGINAL digest, and 3.28.1 must be updated"
    );
}

#[test]
fn this_run_wins_on_a_confirmed_conflict() {
    let old = digest("old");
    let new = digest("new");
    let destination = root_document(POINTER, &[("3.28.1", &old)]);
    let source = source_value(&[("3.28.1", &new)]);

    let merged = merge_root_tags(&source, Some(&destination), &confirmed(&["3.28.1"])).expect("merge");

    assert_eq!(
        tag_digests(&serialize_root(&merged)),
        vec![("3.28.1".to_string(), new.to_string())]
    );
}

#[test]
fn a_surviving_tag_keeps_the_sibling_fields_the_typed_shape_does_not_model() {
    // `RootTag` models `content` + `yanked` and nothing else, so re-emitting a
    // surviving tag through `IndexRoot` would drop the `observed` timestamp
    // every hosted root carries. The merge stays a `Value` operation on both
    // sides precisely so it cannot.
    let destination = root_document(POINTER, &[("latest", &digest("kept"))]);
    let source = source_value(&[("3.28.1", &digest("fresh"))]);

    let merged = merge_root_tags(&source, Some(&destination), &confirmed(&["3.28.1"])).expect("merge");

    assert_eq!(
        merged["tags"]["latest"]["observed"],
        json!("2026-08-14T00:00:00Z"),
        "the surviving tag lost a field the typed shape does not model"
    );
}

#[test]
fn a_source_tag_that_is_not_confirmed_never_appears() {
    // *Confirmed*, not *source*: a tag whose content copy failed must not be
    // published, or the mirror advertises a digest it does not hold.
    let source = source_value(&[("3.28.1", &digest("a")), ("3.29.0", &digest("b"))]);

    let merged = merge_root_tags(&source, None, &confirmed(&["3.28.1"])).expect("merge");

    assert_eq!(
        tag_digests(&serialize_root(&merged)),
        vec![("3.28.1".to_string(), digest("a").to_string())]
    );
}

#[test]
fn package_level_fields_come_from_the_fresh_source_fetch() {
    let destination: Value =
        serde_json::from_slice(&root_document(POINTER, &[("latest", &digest("kept"))])).expect("parse");
    let mut stale = destination.clone();
    stale["desc"] = json!("a description the source has since changed");
    let stale = serialize_root(&stale);

    let mut source = source_value(&[("latest", &digest("kept"))]);
    source["desc"] = json!("the current description");
    source["a_field_from_a_newer_writer"] = json!(["x"]);

    let merged = merge_root_tags(&source, Some(&stale), &confirmed(&["latest"])).expect("merge");

    assert_eq!(merged["desc"], json!("the current description"));
    assert_eq!(merged["a_field_from_a_newer_writer"], json!(["x"]));
}

#[test]
fn the_merge_leaves_the_repository_pointer_alone() {
    // C-028 owns `repository`; the merge is only about `tags`. Splitting them
    // is what keeps each testable on its own.
    let source = source_value(&[("latest", &digest("a"))]);

    let merged = merge_root_tags(&source, None, &confirmed(&["latest"])).expect("merge");

    assert_eq!(merged["repository"], json!("oci://ghcr.io/ocx-sh/cmake"));
}

#[test]
fn a_first_run_degrades_to_the_confirmed_set() {
    let source = source_value(&[("3.28.1", &digest("a"))]);

    let merged = merge_root_tags(&source, None, &confirmed(&["3.28.1"])).expect("merge");

    assert_eq!(
        tag_digests(&serialize_root(&merged)),
        vec![("3.28.1".to_string(), digest("a").to_string())]
    );
}

#[test]
fn a_destination_root_with_no_tags_key_is_not_an_error() {
    // `IndexRoot::tags` is `#[serde(default)]`, so a root without the key is
    // legal — and has nothing to preserve.
    let destination = serialize_root(&json!({ "repository": POINTER, "name": "cmake" }));
    let source = source_value(&[("3.28.1", &digest("a"))]);

    let merged = merge_root_tags(&source, Some(&destination), &confirmed(&["3.28.1"])).expect("merge");

    assert_eq!(
        tag_digests(&serialize_root(&merged)),
        vec![("3.28.1".to_string(), digest("a").to_string())]
    );
}

#[test]
fn a_non_object_tags_field_is_refused_rather_than_read_as_empty() {
    // Reading it as empty would drop every destination tag — the deletion this
    // whole contract exists to prevent — and would do it silently.
    let destination = serialize_root(&json!({ "repository": POINTER, "tags": [] }));
    let source = source_value(&[("3.28.1", &digest("a"))]);

    let error =
        merge_root_tags(&source, Some(&destination), &confirmed(&["3.28.1"])).expect_err("a JSON array is not tags");

    assert!(refusal_message(&error).contains("non-object tags"), "{error:?}");
}

#[test]
fn an_unparseable_destination_root_fails_rather_than_degrading() {
    // The caller must not collapse `Ok(None)` and `Err(_)`; neither may this.
    let source = source_value(&[("3.28.1", &digest("a"))]);

    let error = merge_root_tags(&source, Some(b"{truncated"), &confirmed(&["3.28.1"]))
        .expect_err("a corrupt destination root must fail the package");

    assert!(refusal_message(&error).contains("destination root"), "{error:?}");
}

#[test]
fn pointer_drift_names_the_published_pointer_only_when_it_moved() {
    let published = root_document(POINTER, &[("latest", &digest("a"))]);

    // GREEN — the pointer this run computes matches what is published.
    assert_eq!(pointer_drift(Some(&published), POINTER), None);

    // RED — `destination:` or `target` was edited after first publish.
    assert_eq!(
        pointer_drift(Some(&published), "oci://registry.test/other/tools/cmake"),
        Some(POINTER.to_string())
    );

    // A first run has nothing to compare against, and bytes the merge is about
    // to reject loudly do not get a second, quieter message.
    assert_eq!(pointer_drift(None, POINTER), None);
    assert_eq!(pointer_drift(Some(b"{truncated"), POINTER), None);
}

#[test]
fn a_tag_the_source_dropped_lives_at_the_destination_forever() {
    // The accepted trap, pinned so a later change cannot quietly introduce a
    // delete verb: the index is append-only by ruling.
    let destination = root_document(POINTER, &[("3.27.0", &digest("retired")), ("latest", &digest("a"))]);
    let source = source_value(&[("latest", &digest("a"))]);

    let merged = merge_root_tags(&source, Some(&destination), &confirmed(&["latest"])).expect("merge");

    assert_eq!(
        tag_digests(&serialize_root(&merged)),
        vec![
            ("3.27.0".to_string(), digest("retired").to_string()),
            ("latest".to_string(), digest("a").to_string()),
        ]
    );
}
