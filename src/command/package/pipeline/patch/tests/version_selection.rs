// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── Version selection ─────────────────────────────────────────────────

/// A build-stamped mirror is the case the bounds have to survive:
/// `Version` sorts `3.29.0_20260610` BEFORE `3.29.0`, so comparing the
/// published version directly would drop every stamped tag out of an
/// inclusive lower bound naming its own core.
#[test]
fn min_version_is_inclusive_on_the_version_core() {
    let tags = ["3.28.0_20260101", "3.29.0_20260610", "3.30.0_20260701"];
    let selection = Selection::parse(&[], Some("3.29.0"), None).expect("bounds parse");

    assert_eq!(
        selected_tags(&selection, &tags),
        vec!["3.29.0_20260610", "3.30.0_20260701"],
    );
}

#[test]
fn max_version_is_exclusive() {
    let tags = ["3.28.0", "3.29.0", "3.30.0"];
    let selection = Selection::parse(&[], None, Some("3.30.0")).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.28.0", "3.29.0"]);
}

/// The exclusive upper bound must exclude every build of the version it
/// names, not just the bare tag — otherwise `--max-version 3.30.0` still
/// patches `3.30.0_20260701` on exactly the mirrors that stamp builds.
#[test]
fn max_version_excludes_every_build_of_the_version_it_names() {
    let tags = ["3.29.0_20260610", "3.30.0_20260701"];
    let selection = Selection::parse(&[], None, Some("3.30.0")).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0_20260610"]);
}

#[test]
fn both_bounds_narrow_to_a_half_open_window() {
    let tags = ["3.28.0", "3.29.0", "3.30.0", "3.31.0"];
    let selection = Selection::parse(&[], Some("3.29.0"), Some("3.31.0")).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0", "3.30.0"]);
}

#[test]
fn no_selector_at_all_patches_every_published_version() {
    let tags = ["3.28.0", "3.29.0", "3.30.0"];
    let selection = Selection::parse(&[], None, None).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.28.0", "3.29.0", "3.30.0"]);
}

#[test]
fn an_exact_version_selects_only_itself() {
    let tags = ["3.28.0", "3.29.0", "3.30.0"];
    let selection = Selection::parse(&["3.29.0".to_string()], None, None).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0"]);
}

/// The version a human reads off the spec is the core; the leaf tag carries
/// the stamp. Requiring the stamp would make `--version` unusable on every
/// mirror that has one.
#[test]
fn an_exact_version_matches_a_build_stamped_leaf_by_its_core() {
    let tags = ["3.29.0_20260610", "3.30.0_20260701"];
    let selection = Selection::parse(&["3.29.0".to_string()], None, None).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0_20260610"]);
}

#[test]
fn an_exact_version_composes_with_a_range_as_a_union() {
    let tags = ["3.28.0", "3.29.0", "3.30.0", "3.31.0"];
    let selection = Selection::parse(&["3.28.0".to_string()], Some("3.30.0"), None).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.28.0", "3.30.0", "3.31.0"]);
}

/// Silently patching nothing is how a corrected spec stays unpublished
/// while the run reports success.
#[test]
fn an_unpublished_exact_version_is_a_usage_error() {
    use ocx_lib::cli::ExitCode;

    let selection = Selection::parse(&["9.9.9".to_string()], None, None).expect("bounds parse");
    let error = selection.apply(&tag_list(&["3.29.0"])).expect_err("must reject");

    assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "got: {error}");
}

#[test]
fn an_unparseable_bound_is_a_usage_error() {
    use ocx_lib::cli::ExitCode;

    let error = Selection::parse(&[], Some("three"), None).expect_err("must reject");
    assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "got: {error}");
}

/// Aliases share their leaf's child manifests, so scanning them would
/// schedule the same patch four times — and patching the leaf re-cascades
/// them anyway.
#[test]
fn only_leaf_tags_are_selectable_never_their_cascade_aliases() {
    let tags = ["3.29.0_20260610", "3.29.0", "3.29", "3", "latest"];
    let selection = Selection::parse(&[], None, None).expect("bounds parse");

    assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0_20260610"]);
}

/// The alias is not silently skipped either — naming one is a usage error,
/// so nobody concludes their patch ran.
#[test]
fn naming_a_cascade_alias_exactly_is_a_usage_error() {
    let selection = Selection::parse(&["3.29".to_string()], None, None).expect("bounds parse");
    let error = selection
        .apply(&tag_list(&["3.29.0_20260610", "3.29.0", "3.29", "3"]))
        .expect_err("must reject");

    assert!(error.to_string().contains("3.29"), "got: {error}");
}
