// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Unit tests for the glob engine (C-009) and package selection (C-010).

use super::*;

#[test]
fn compile_with_no_star_matches_only_the_exact_name() {
    let glob = Glob::compile("kitware/cmake").unwrap();
    assert!(glob.matches("kitware/cmake"));
    assert!(!glob.matches("kitware/cmake2"));
    assert!(!glob.matches("xkitware/cmake"));
    assert!(!glob.matches(""));
}

#[test]
fn empty_pattern_matches_only_the_empty_string() {
    let glob = Glob::compile("").unwrap();
    assert!(glob.matches(""));
    assert!(!glob.matches("kitware/cmake"));
}

#[test]
fn star_alone_matches_everything() {
    let glob = Glob::compile("*").unwrap();
    assert!(glob.matches("kitware/cmake"));
    assert!(glob.matches(""));
    assert!(glob.matches("anything/at/all"));
}

#[test]
fn multiple_non_adjacent_stars_are_allowed_and_each_matches_independently() {
    // The `**` rejection is about two ADJACENT stars, not "more than one
    // star anywhere in the pattern" — a naive version of that check would
    // wrongly reject this.
    let glob = Glob::compile("a*b*c").unwrap();
    assert!(glob.matches("aXXbYYc"));
    assert!(glob.matches("abc"));
    assert!(!glob.matches("acb"));
}

#[test]
fn star_fills_one_segment_and_is_anchored_to_the_literal_prefix() {
    // C-009's own edge case: `kitware/*` matches `kitware/cmake` and must not
    // match `kitware2/cmake` — the literal prefixes `kitware/` and
    // `kitware2/` diverge at the `/`, so the anchor alone does the work, with
    // no special-casing for segment boundaries needed.
    let glob = Glob::compile("kitware/*").unwrap();
    assert!(glob.matches("kitware/cmake"));
    assert!(!glob.matches("kitware2/cmake"));
}

#[test]
fn star_matches_a_run_of_characters_within_one_segment() {
    let glob = Glob::compile("kitware/c*e").unwrap();
    assert!(glob.matches("kitware/cmake"));
    assert!(glob.matches("kitware/ce"));
    assert!(!glob.matches("kitware/cmak"));
}

#[test]
fn star_matches_across_the_segment_boundary_too() {
    // The subject is the whole two-segment name, not one path segment: `*`
    // matches any run of characters including `/`, which is what lets one
    // include entry stand in for a namespace-spanning selection.
    let glob = Glob::compile("*/cmake").unwrap();
    assert!(glob.matches("kitware/cmake"));
    assert!(glob.matches("anything/cmake"));
}

#[test]
fn anchoring_requires_a_full_match_not_a_substring() {
    // `foo/bar` must not be matched by treating `oo/ba` as a substring of it.
    let glob = Glob::compile("oo/ba").unwrap();
    assert!(!glob.matches("foo/bar"));
    assert!(glob.matches("oo/ba"));
}

#[test]
fn regex_metacharacters_in_a_literal_segment_match_literally_not_as_regex() {
    // This is the bug `regex::escape` exists to prevent: unescaped, `.`
    // means "any character" and `+` means "one or more of the preceding
    // character" — both would make this glob match strings the operator
    // never wrote.
    let dot = Glob::compile("a.b/c").unwrap();
    assert!(dot.matches("a.b/c"));
    assert!(
        !dot.matches("axb/c"),
        "the literal '.' must not act as regex any-character"
    );

    let dot_and_plus = Glob::compile("a.b/c+d").unwrap();
    assert!(dot_and_plus.matches("a.b/c+d"));
    assert!(
        !dot_and_plus.matches("axb/cccd"),
        "the literal '.' and '+' must not act as regex metacharacters"
    );
}

#[test]
fn compile_rejects_double_star_question_mark_and_brace() {
    let cases = [("a**b", '*'), ("a?b/c", '?'), ("{a,b}/c", '{')];
    for (pattern, expected_character) in cases {
        let error = Glob::compile(pattern).expect_err("a pattern outside the grammar must be refused, not accepted");
        assert_eq!(
            error,
            GlobError::UnsupportedMetacharacter {
                pattern: pattern.to_string(),
                character: expected_character,
            },
            "pattern {pattern:?}"
        );
    }
}

#[test]
fn an_invalid_pattern_surfaces_as_an_error_not_a_panic() {
    // `Glob::compile` returning `Err` (rather than panicking) is the contract
    // under test; formatting that error must not panic either.
    let error = Glob::compile("a**b").unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("a**b"),
        "message must name the offending pattern: {message}"
    );
    assert!(
        message.contains('*'),
        "message must name the offending character: {message}"
    );
}

#[test]
fn package_selected_with_empty_include_admits_everything() {
    assert!(package_selected("kitware/cmake", &[], &[]));
    assert!(package_selected("anything/at-all", &[], &[]));
}

#[test]
fn package_selected_requires_an_include_match_when_include_is_non_empty() {
    let include = [Glob::compile("kitware/*").unwrap()];
    assert!(package_selected("kitware/cmake", &include, &[]));
    assert!(!package_selected("other/package", &include, &[]));
}

#[test]
fn package_selected_exclude_vetoes_an_included_match() {
    // Exclude is an unconditional veto: a name matching both an include and
    // an exclude is rejected, not admitted.
    let include = [Glob::compile("kitware/*").unwrap()];
    let exclude = [Glob::compile("kitware/cmake").unwrap()];
    assert!(!package_selected("kitware/cmake", &include, &exclude));
    // A sibling package under the same include, not named by the exclude,
    // still passes — the veto is per-name, not per-include-list.
    assert!(package_selected("kitware/ninja", &include, &exclude));
}

#[test]
fn package_selected_exclude_applies_even_with_empty_include() {
    let exclude = [Glob::compile("kitware/cmake").unwrap()];
    assert!(!package_selected("kitware/cmake", &[], &exclude));
    assert!(package_selected("kitware/ninja", &[], &exclude));
}
