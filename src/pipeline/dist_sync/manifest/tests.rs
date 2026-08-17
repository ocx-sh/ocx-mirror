// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::*;
use crate::spec::Select;

/// Three releases across both channels, newest-first, in the shape
/// `gen-dist.sh` emits.
const UPSTREAM: &str = r#"{
  "schema": 1,
  "latest": {"version":"0.5.8","channel":"stable"},
  "latest_next": {"version":"0.6.0-rc.1","channel":"next"},
  "releases": [
    {"version":"0.6.0-rc.1","channel":"next","tag":"v0.6.0-rc.1","target":"x86_64-unknown-linux-gnu","filename":"ocx-x86_64-unknown-linux-gnu.tar.gz","sha256":"aa","url":"https://example.test/a"},
    {"version":"0.5.8","channel":"stable","tag":"v0.5.8","target":"aarch64-apple-darwin","filename":"ocx-aarch64-apple-darwin.tar.gz","sha256":"bb","url":"https://example.test/b"},
    {"version":"0.5.8","channel":"stable","tag":"v0.5.8","target":"x86_64-unknown-linux-gnu","filename":"ocx-x86_64-unknown-linux-gnu.tar.gz","sha256":"cc","url":"https://example.test/c"},
    {"version":"0.4.0","channel":"stable","tag":"v0.4.0","target":"x86_64-unknown-linux-gnu","filename":"ocx-x86_64-unknown-linux-gnu.tar.gz","sha256":"dd","url":"https://example.test/d"}
  ]
}"#;

fn parsed() -> DistManifest {
    serde_json::from_str(UPSTREAM).expect("the fixture must parse")
}

fn select(min: Option<&str>) -> Select {
    Select {
        min_version: min.map(str::to_string),
    }
}

/// `install.sh`'s `get_latest_version` and `dist_row` extract leaf objects
/// with `grep -o '{[^{}]*}'`, which is line-based. This runs the same
/// extraction, so a rendering change that splits an object across lines fails
/// here instead of silently breaking every jq-free installer.
///
/// Replica of the parse as it stood in `ocx-sh/www-setup` at v1.3.0; that
/// repository shares no CI with this one, so this side pins the contract.
fn leaf_objects(rendered: &str) -> Vec<String> {
    let pattern = regex::Regex::new(r"\{[^{}]*\}").expect("the installer's own pattern must compile");
    rendered
        .lines()
        .flat_map(|line| {
            pattern
                .find_iter(line)
                .map(|found| found.as_str().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The whole reason the rendering is hand-rolled. A leaf object split across
/// lines is invisible to every POSIX installer, and the failure is silent.
#[test]
fn the_posix_extraction_finds_every_leaf_object() {
    let manifest = parsed();
    let rendered = manifest.render().expect("rendering must succeed");

    // Two pointers plus one per release row.
    let expected = 2 + manifest.releases.len();

    assert_eq!(
        leaf_objects(&rendered).len(),
        expected,
        "`grep -o '{{[^{{}}]*}}'` must see every leaf object:\n{rendered}"
    );
}

/// `get_latest_version` takes the **first** leaf object whose channel is
/// `stable`. Emitting the pointers after `releases` would silently resolve the
/// newest stable *release row* instead — identical today, wrong the moment a
/// filter re-points `latest`.
#[test]
fn the_latest_pointer_is_the_first_stable_leaf_object_in_the_document() {
    let mut manifest = parsed();
    manifest.apply_select(&select(None));
    let rendered = manifest.render().expect("rendering must succeed");

    let first_stable = rendered
        .lines()
        .find(|line| line.contains(r#""channel":"stable""#))
        .expect("a stable leaf object must be present");

    assert!(
        first_stable.contains(r#""version":"0.5.8""#),
        "the first stable leaf object must be the latest pointer: {first_stable}"
    );
}

#[test]
fn a_min_version_bound_is_inclusive() {
    let mut manifest = parsed();
    manifest.apply_select(&select(Some("0.5.8")));

    let versions: Vec<&str> = manifest.releases.iter().map(|row| row.version.as_str()).collect();

    assert!(
        versions.contains(&"0.5.8"),
        "the bound itself must survive: {versions:?}"
    );
    assert!(
        !versions.contains(&"0.4.0"),
        "an older release must be dropped: {versions:?}"
    );
}

/// Semver orders a prerelease below its own release, so `min_version: 1.0.0`
/// excludes `1.0.0-rc.1`. Stated as a test because it is the one part of the
/// bound operators consistently expect to go the other way.
#[test]
fn a_min_version_bound_excludes_the_prerelease_of_that_same_version() {
    let mut manifest = parsed();
    manifest.apply_select(&select(Some("0.6.0")));

    let versions: Vec<&str> = manifest.releases.iter().map(|row| row.version.as_str()).collect();

    assert!(
        !versions.contains(&"0.6.0-rc.1"),
        "0.6.0-rc.1 sorts below 0.6.0 and must be excluded: {versions:?}"
    );
}

#[test]
fn filtering_re_points_latest_at_the_newest_survivor() {
    let mut manifest = parsed();
    manifest.apply_select(&select(Some("0.4.0")));
    // Drop every 0.5.8 and 0.6.0 row by hand, leaving 0.4.0 as the newest —
    // the shape a `max_version` or `targets` filter will produce later.
    manifest.releases.retain(|row| row.version == "0.4.0");
    manifest.apply_select(&select(None));

    assert_eq!(
        manifest.latest.as_ref().map(|pointer| pointer.version.as_str()),
        Some("0.4.0"),
        "latest must name a release the mirror actually holds"
    );
}

#[test]
fn a_channel_that_empties_leaves_a_null_pointer() {
    let mut manifest = parsed();
    manifest.releases.retain(|row| row.channel == "stable");
    manifest.apply_select(&select(None));

    assert!(
        manifest.latest_next.is_none(),
        "an empty channel must be null, not a pointer at a release that is gone"
    );

    let rendered = manifest.render().expect("rendering must succeed");
    assert!(
        rendered.contains(r#""latest_next": null"#),
        "the null must be explicit in the document: {rendered}"
    );
}

#[test]
fn an_unfiltered_manifest_keeps_the_upstream_pointer_objects() {
    let mut manifest = parsed();
    let before = manifest.latest.clone();
    manifest.apply_select(&select(None));

    assert_eq!(
        manifest.latest, before,
        "re-pointing must be a no-op when the newest survivor is what upstream already named"
    );
}

/// The mirror is the one hop with no way to know who reads what, so a field it
/// does not understand must survive the round trip.
#[test]
fn unknown_keys_survive_the_round_trip() {
    let source = r#"{
  "schema": 1,
  "latest": null,
  "latest_next": null,
  "releases": [
    {"version":"1.0.0","channel":"stable","tag":"v1.0.0","target":"t","filename":"f","sha256":"ab","url":"https://example.test/f","signature":"sig"}
  ],
  "generated_at": "2026-08-17T00:00:00Z"
}"#;

    let manifest: DistManifest = serde_json::from_str(source).expect("the fixture must parse");
    let rendered = manifest.render().expect("rendering must succeed");

    assert!(
        rendered.contains(r#""signature":"sig""#),
        "an unknown row key must survive"
    );
    assert!(
        rendered.contains(r#""generated_at""#),
        "an unknown top-level key must survive"
    );
}

/// Unknown top-level keys ride *after* `releases`: an object emitted above the
/// pointers would become the first leaf object `get_latest_version` sees.
#[test]
fn an_unknown_top_level_object_cannot_shadow_the_latest_pointer() {
    let source = r#"{
  "schema": 1,
  "aaa_sorts_first": {"channel":"stable","version":"9.9.9"},
  "latest": {"version":"1.0.0","channel":"stable"},
  "latest_next": null,
  "releases": []
}"#;

    let manifest: DistManifest = serde_json::from_str(source).expect("the fixture must parse");
    let rendered = manifest.render().expect("rendering must succeed");

    let first_stable = rendered
        .lines()
        .find(|line| line.contains(r#""channel":"stable""#))
        .expect("a stable leaf object must be present");

    assert!(
        first_stable.contains(r#""version":"1.0.0""#),
        "the latest pointer must still be the first stable leaf object: {first_stable}"
    );
}

#[test]
fn an_empty_release_list_still_renders_valid_json() {
    let mut manifest = parsed();
    manifest.releases.clear();
    manifest.apply_select(&select(None));

    let rendered = manifest.render().expect("rendering must succeed");

    serde_json::from_str::<serde_json::Value>(&rendered)
        .unwrap_or_else(|error| panic!("an empty manifest must be valid JSON: {error}\n{rendered}"));
}

#[test]
fn a_rendered_manifest_round_trips_through_the_parser() {
    let rendered = parsed().render().expect("rendering must succeed");

    let reparsed: DistManifest = serde_json::from_str(&rendered).expect("the mirror's own output must parse");

    assert_eq!(reparsed.releases.len(), 4);
    assert_eq!(reparsed.schema, SUPPORTED_SCHEMA);
}

#[test]
fn a_digest_that_is_not_sixty_four_hex_characters_is_refused() {
    let mut manifest = parsed();
    // The fixture's two-character digests are exactly this case.
    let error = manifest.releases[0]
        .digest()
        .expect_err("a short digest must be refused");
    assert!(error.contains("64 hex"), "{error}");

    manifest.releases[0].sha256 = "0".repeat(64);
    assert_eq!(
        manifest.releases[0].digest().expect("a well-formed digest is accepted"),
        format!("sha256:{}", "0".repeat(64))
    );
}

#[test]
fn a_digest_is_lowercased_before_it_becomes_an_oci_digest_string() {
    let mut manifest = parsed();
    manifest.releases[0].sha256 = "A".repeat(64);

    assert_eq!(
        manifest.releases[0].digest().expect("uppercase hex is still hex"),
        format!("sha256:{}", "a".repeat(64))
    );
}

/// A manifest is remote input, and a `file://` row would make it able to read
/// the mirror host's own filesystem into the published tree.
#[test]
fn a_row_url_that_is_not_http_is_refused() {
    let mut manifest = parsed();
    manifest.releases[0].url = "file:///etc/passwd".to_string();

    let error = manifest.releases[0].check_url().expect_err("file:// must be refused");

    assert!(error.contains("not http or https"), "{error}");
}

/// CWE-117. `version` and `target` come off a foreign manifest and land in the
/// report table and in every message naming the row — and the default
/// `publish.layout` substitutes neither, so `LayoutTemplate`'s refusal, where
/// every other foreign value is checked, never sees these two.
#[test]
fn a_row_label_escapes_a_value_that_would_forge_a_log_line() {
    let mut manifest = parsed();
    manifest.releases[0].version = "0.5.8\n[ERROR] everything is fine".to_string();
    manifest.releases[0].target = "x86_64\r\nok".to_string();

    let label = manifest.releases[0].label();

    assert!(
        !label.contains('\n'),
        "a newline must not reach the log verbatim: {label}"
    );
    assert!(!label.contains('\r'), "a carriage return must not either: {label}");
    assert!(
        label.starts_with("0.5.8\\n"),
        "the value is escaped, not truncated: {label}"
    );
}

/// The escaping must not disfigure the labels operators actually read.
#[test]
fn an_ordinary_row_label_is_unchanged() {
    assert_eq!(parsed().releases[0].label(), "0.6.0-rc.1/x86_64-unknown-linux-gnu");
}
