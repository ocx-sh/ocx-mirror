// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::*;

/// A row with values that are all safe path components.
fn row() -> RowValues<'static> {
    RowValues {
        version: "0.5.8",
        tag: "v0.5.8",
        target: "x86_64-unknown-linux-gnu",
        filename: "ocx-x86_64-unknown-linux-gnu.tar.gz",
        channel: "stable",
    }
}

#[test]
fn the_default_layout_reproduces_the_installers_own_mirror_rewrite() {
    // `install.sh` composes `${OCX_INSTALL_MIRROR_URL}/${tag}/${filename}`.
    // A default that disagreed would make a mirror emitted by this tool
    // unreachable by a consumer that sets the mirror knob instead of reading
    // the rewritten manifest.
    let template = LayoutTemplate::parse("{tag}/{filename}").expect("the default template must parse");

    assert_eq!(
        template.expand(&row()).expect("a safe row must expand"),
        "v0.5.8/ocx-x86_64-unknown-linux-gnu.tar.gz"
    );
}

#[test]
fn the_gitlab_generic_layout_drops_the_v_prefixed_tag_segment() {
    // The reason `layout:` is configurable at all: a GitLab generic package
    // registry addresses files as `<package>/<version>/<file>`, which the
    // consumers' hardcoded `<tag>/<filename>` cannot express.
    let template = LayoutTemplate::parse("{version}/{filename}").expect("template must parse");

    assert_eq!(
        template.expand(&row()).expect("a safe row must expand"),
        "0.5.8/ocx-x86_64-unknown-linux-gnu.tar.gz"
    );
}

#[test]
fn every_placeholder_is_substitutable() {
    let template = LayoutTemplate::parse("{channel}/{version}/{tag}/{target}/{filename}").expect("template must parse");

    assert_eq!(
        template.expand(&row()).expect("a safe row must expand"),
        "stable/0.5.8/v0.5.8/x86_64-unknown-linux-gnu/ocx-x86_64-unknown-linux-gnu.tar.gz"
    );
}

#[test]
fn a_template_with_no_placeholders_is_a_literal_path() {
    let template = LayoutTemplate::parse("archives/ocx.tar.gz").expect("template must parse");

    assert_eq!(
        template.expand(&row()).expect("expansion must succeed"),
        "archives/ocx.tar.gz"
    );
}

#[test]
fn an_unknown_placeholder_is_refused_rather_than_expanded_to_nothing() {
    // The closed set is the point: an unknown name silently expanding to the
    // empty string would collapse a path segment and quietly collide every
    // release onto one path.
    let error = LayoutTemplate::parse("{tag}/{sha256}").expect_err("an unknown placeholder must be refused");

    assert_eq!(
        error,
        LayoutError::UnknownPlaceholder {
            name: "sha256".to_string()
        }
    );
}

/// The substituted values are guarded per value, but the template's own
/// literals were not — and `Path::join` discards its base when handed an
/// absolute path, so `/etc/{filename}` wrote outside `output:` entirely.
#[test]
fn a_template_whose_literals_escape_the_output_directory_is_refused() {
    for hostile in [
        "/etc/cron.d/{filename}",
        "\\windows\\{filename}",
        "../{tag}/{filename}",
        "{tag}/../../{filename}",
        "./{filename}",
    ] {
        let outcome = LayoutTemplate::parse(hostile);

        assert!(
            matches!(outcome, Err(LayoutError::EscapingTemplate { .. })),
            "{hostile} must be refused, got {outcome:?}"
        );
    }
}

#[test]
fn a_relative_template_with_ordinary_literals_is_accepted() {
    // The guard must not reject the layouts operators actually write — in
    // particular a dot inside a segment, which is not a `.` segment.
    for benign in [
        "{tag}/{filename}",
        "archives/{version}/{filename}",
        "ocx.dist/{filename}",
    ] {
        assert!(LayoutTemplate::parse(benign).is_ok(), "{benign} must be accepted");
    }
}

#[test]
fn an_unterminated_placeholder_is_refused() {
    let error = LayoutTemplate::parse("{tag}/{filename").expect_err("an unterminated brace must be refused");

    assert!(matches!(error, LayoutError::UnterminatedPlaceholder { .. }));
}

/// The containment boundary. Every value here arrives from a manifest fetched
/// over the network, so each case is a directory traversal or a collapsed
/// segment that a *repair* would hide rather than report.
#[test]
fn a_manifest_value_that_would_escape_the_output_directory_is_refused() {
    let template = LayoutTemplate::parse("{tag}/{filename}").expect("template must parse");

    for (label, hostile) in [
        ("parent traversal", "../../etc/cron.d/pwn"),
        ("bare parent", ".."),
        ("current directory", "."),
        ("empty", ""),
        ("absolute path", "/etc/passwd"),
        ("windows separator", "..\\windows\\system32"),
        ("embedded nul", "ocx\0.tar.gz"),
    ] {
        let mut row = row();
        row.filename = hostile;

        let outcome = template.expand(&row);

        assert!(outcome.is_err(), "{label} must be refused, but expanded to {outcome:?}");
    }
}

#[test]
fn the_refusal_names_the_manifest_field_the_hostile_value_arrived_in() {
    let template = LayoutTemplate::parse("{tag}/{filename}").expect("template must parse");
    let mut hostile = row();
    hostile.tag = "../..";

    let error = template.expand(&hostile).expect_err("a traversal must be refused");

    assert_eq!(
        error,
        LayoutError::UnsafeValue {
            field: "tag",
            value: "../..".to_string()
        }
    );
}

#[test]
fn a_refusal_message_escapes_a_value_that_would_forge_a_log_line() {
    // CWE-117: these values come off a foreign manifest and land in the CI
    // output an operator reads.
    let error = LayoutError::UnsafeValue {
        field: "filename",
        value: "ocx\n[ERROR] everything is fine".to_string(),
    };

    let rendered = error.to_string();

    assert!(
        !rendered.contains('\n'),
        "a newline in a manifest value must not reach the log verbatim: {rendered}"
    );
}
