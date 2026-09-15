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

/// The default-variant alias is `Publisher`'s own `default` track, not a
/// second push.
///
/// ocx 0.6.2 re-tags the pushed manifest onto the bare version track inside
/// `push_cascade` (`default: true`), through the index alone. The earlier
/// shape — a second `push_cascade` under a variant-less identifier —
/// re-uploaded one manifest per platform and, had it signed, would have spent
/// a duplicate referrer against the verifier's cap (a signature is a referrer
/// on the subject digest, not the tag). One cascade call, one signing call,
/// the flag derived from the variant context: that is the whole shape.
///
/// Structural rather than behavioural because the property is the *absence*
/// of a call on a path that needs a live `Publisher` and a variant-carrying
/// registry fixture to reach.
#[test]
fn the_default_variant_alias_is_the_publishers_default_track_not_a_second_push() {
    let source = source_without_comments();

    assert_eq!(
        source.matches("push_cascade(").count(),
        1,
        "one cascade push per platform; the bare alias rides on its `default` flag"
    );
    assert!(
        source.contains("let default = variant.is_some_and(|ctx| ctx.is_default);"),
        "the default flag is derived from the variant context"
    );
    // The derived flag, not its negation or a literal, is what the cascade
    // receives: pinned on the argument list itself, whitespace ignored.
    let flat: String = source.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        flat.contains("canonical_tag,default,annotations,"),
        "`default` is passed to push_cascade as derived"
    );
    assert_eq!(
        source.matches("sign_platform(sign, &signed_ref").count(),
        2,
        "one signing call per branch (cascade and plain), none for an alias"
    );
}
