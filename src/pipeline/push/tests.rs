// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Guards on [`super::push_and_cascade`]'s signing shape.

/// This module's own source, comments stripped.
///
/// A signing call quoted in a doc comment must not satisfy — or defeat — the
/// scan below (TEST-11).
fn source_without_comments() -> String {
    include_str!("../push.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The default-variant alias block signs nothing of its own.
///
/// A Sigstore signature is a referrer against the **subject digest**, not the
/// tag. `test_default_variant_aliases_the_bare_tags_to_its_own_manifest` pins
/// that every bare tag resolves to the default variant's own manifest, so the
/// alias push lands on a digest the version tag's own `sign_platform` below
/// already covers. A second call there is a duplicate referrer spending a
/// candidate against ocx's verifier cap, not a gap being closed.
///
/// Structural rather than behavioural because the property is the *absence*
/// of a call on a path that needs a live `Publisher` and a variant-carrying
/// registry fixture to reach.
#[test]
fn the_bare_alias_push_signs_nothing_of_its_own() {
    let source = source_without_comments();

    // The needle is live: an absence assertion over a pattern that matches
    // nothing anywhere reports green forever.
    assert!(
        source.matches("sign_platform(").count() >= 2,
        "sign_platform is no longer spelled this way; this guard scans for nothing"
    );

    let open = source
        .find("vec![bare_info]")
        .expect("the default-variant alias push names bare_info");
    let close = source
        .find("sign_platform(sign, &signed_ref")
        .expect("the version tag's platform manifest is signed after the alias push");
    assert!(open < close, "the alias push precedes the version tag's signing call");

    let alias_block = &source[open..close];
    assert!(
        !alias_block.contains("sign_platform("),
        "the bare alias push signs its own reference; that is a duplicate \
         referrer on the digest the version tag's call already covers"
    );
}
