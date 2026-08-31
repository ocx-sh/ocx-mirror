// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Unit tests for the orchestrator (C-040, C-044).
//!
//! Most of this module is a *sequence*, and a sequence against a live source
//! and a live destination is WP-16's acceptance surface, not a unit test's.
//! What is testable here is what the sequence would get wrong silently:
//!
//! - **Phase 0**, behaviourally — [`bootstrap`] against an `output:` that does
//!   not exist yet. That is the one bug every other test would miss, because
//!   every other `output:` already exists.
//! - **The two error classes** (C-040) — [`tag_failure`] and [`write_failure`]
//!   are the whole of the classification, table-driven over every variant so a
//!   new one is classified consciously rather than by fall-through.
//! - **The counters and the report rows**, which are what an operator and CI
//!   actually read.
//! - **The call order**, structurally. Dispatch-objects-then-root and
//!   commit-then-`config.json` are correctness, not style, and nothing short
//!   of a registry can exercise them — so they are asserted on the source
//!   text, with comments stripped first.

use std::path::Path;

use super::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// This module's own source text with every comment line removed.
///
/// Load-bearing: the doc comments in `registry_sync.rs` deliberately *name*
/// the calls and orderings the structural guards below check — which is the
/// right thing for a comment to do, and exactly what makes an unstripped
/// assertion match its own documentation and pass vacuously.
fn module_source_without_comments() -> String {
    include_str!("../registry_sync.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of one `fn` in the stripped source, from its signature to the
/// start of the next top-level item.
fn function_body(name: &str) -> String {
    let source = module_source_without_comments();
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("{name} is not defined in registry_sync.rs"));
    let rest = &source[start..];
    // Top-level items start at column 0; the first one after `start` ends the
    // body. `\n}` is the closing brace of the function itself.
    let end = rest.find("\n}\n").map_or(rest.len(), |offset| offset + 2);
    rest[..end].to_string()
}

/// Assert `first` appears before `second` in `body`, naming both when it does
/// not.
fn assert_ordered(body: &str, first: &str, second: &str) {
    let left = body
        .find(first)
        .unwrap_or_else(|| panic!("'{first}' is not called at all"));
    let right = body
        .find(second)
        .unwrap_or_else(|| panic!("'{second}' is not called at all"));
    assert!(
        left < right,
        "'{first}' must be called before '{second}', but it is not"
    );
}

fn options() -> RegistrySyncOptions {
    RegistrySyncOptions {
        dry_run: false,
        fail_fast: false,
        repair_catalog: false,
        cache_dir: None,
        format: crate::command::package::options::OutputFormat::Plain,
    }
}

/// A minimal valid spec whose `output:` is `output`.
fn spec_with_output(output: &Path) -> RegistrySpec {
    let document = format!(
        r#"
target:
  registry: localhost:5002
  repository: mirror
output: {}
destination: "{{registry}}/{{namespace}}/{{package}}"
sources:
  - registry: localhost:5001
    index: http://localhost:8080
    as: upstream
"#,
        output.display()
    );
    serde_yaml_ng::from_str(&document).expect("the test spec parses")
}

fn source_report(as_name: &str, short_circuited: bool, packages: Vec<PackageReport>) -> SourceReport {
    SourceReport {
        as_name: as_name.to_string(),
        short_circuited,
        packages,
    }
}

fn row(name: &str, outcome: PackageOutcome) -> PackageReport {
    PackageReport {
        name: name.to_string(),
        outcome,
        detail: None,
    }
}

// ── Phase 0 — bootstrap (C-044, S-028's unit-level twin) ────────────────────

#[tokio::test]
async fn bootstrap_creates_an_output_tree_that_does_not_exist_yet() {
    let temporary = tempfile::tempdir().expect("tempdir");
    // Two levels deep and absent: `create_dir_all`, not `create_dir`, and the
    // path genuinely missing. A pytest tmpdir already exists, which is why
    // every acceptance scenario passes with this bug present.
    let output = temporary.path().join("fresh").join("tree");
    assert!(!output.exists(), "the test must start from a missing output tree");

    let spec = spec_with_output(&output);
    let (cache_root, _store) = bootstrap(&spec, &options())
        .await
        .expect("bootstrap must create the output tree before canonicalizing it");

    assert!(output.is_dir(), "bootstrap must leave the output tree on disk");
    assert!(
        cache_root.ends_with("ocx-mirror"),
        "the cache root is the ocx-mirror cache directory, got {}",
        cache_root.display()
    );
}

#[tokio::test]
async fn bootstrap_honours_the_cache_dir_override() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let output = temporary.path().join("tree");
    let cache_override = temporary.path().join("cache");

    let spec = spec_with_output(&output);
    let mut options = options();
    options.cache_dir = Some(cache_override.clone());

    let (cache_root, _store) = bootstrap(&spec, &options).await.expect("bootstrap");
    assert_eq!(cache_root, cache_override);
}

#[tokio::test]
async fn bootstrap_reports_an_output_path_it_cannot_create() {
    let temporary = tempfile::tempdir().expect("tempdir");
    // A regular file where a directory has to go: `create_dir_all` fails, and
    // the failure is local, so it is the whole-run abort class (74).
    let occupied = temporary.path().join("not-a-directory");
    std::fs::write(&occupied, b"blocked").expect("write");

    let spec = spec_with_output(&occupied);
    let error = bootstrap(&spec, &options())
        .await
        .expect_err("a file where the output tree belongs must fail");
    assert!(
        matches!(error, MirrorError::IndexWriteError(_)),
        "a local filesystem failure is IndexWriteError (74), got {error:?}"
    );
}

// ── C-040 — the copy-side classifier ────────────────────────────────────────

#[test]
fn every_aggregating_copy_failure_fails_only_its_package() {
    let digest = ocx_lib::oci::Algorithm::Sha256.hash(b"content");
    let aggregating = vec![
        CopyError::DigestMismatch {
            expected: digest.clone(),
            actual: ocx_lib::oci::Algorithm::Sha256.hash(b"other"),
        },
        CopyError::SourceUnavailable("connection reset".to_string()),
        CopyError::MalformedManifest("descriptor digest 'x' is not a digest".to_string()),
        CopyError::PushRejected("507 insufficient storage".to_string()),
        CopyError::ReferrersPresent {
            shown: vec![digest.clone()],
            total: 1,
        },
        CopyError::ContentMissing { content: digest },
    ];

    for error in aggregating {
        let rendered = format!("{error}");
        let message = tag_failure("1.2.3", error).expect("an aggregating failure must not abort the run");
        assert!(
            message.starts_with("tag \"1.2.3\": "),
            "the tag is the context an operator needs, missing from {message}"
        );
        assert!(
            message.contains(&rendered),
            "the failure's own message must survive, got {message}"
        );
    }
}

#[test]
fn an_abort_propagates_its_inner_error_verbatim() {
    // Not re-wrapped: the inner variant is what picks the exit code, and
    // `Abort` carries more than one of them.
    for inner in [
        MirrorError::TargetError("destination answered 401".to_string()),
        MirrorError::ExecutionFailed(vec!["the blob semaphore was closed".to_string()]),
    ] {
        let expected = format!("{inner:?}");
        let error = tag_failure("1.2.3", CopyError::Abort(inner)).expect_err("an abort must abort");
        assert_eq!(
            format!("{error:?}"),
            expected,
            "the inner error must reach the exit-code mapping unchanged"
        );
    }
}

// ── CWE-117 — a foreign tag never reaches a message raw ─────────────────────

/// A tag is authored by the source — it comes straight out of an upstream
/// root's `tags{}` map and passes no charset guard anywhere on this path.
///
/// [`tag_failure`]'s string is the one that becomes `PackageReport.detail`, and
/// an operator reads that twice: once in a CI log, where a newline forges lines
/// they take at face value, and once through `print_table` on their own
/// terminal, where `\u{202e}` reverses what is shown and an ANSI escape is
/// interpreted. Escaping at this one site is what covers both renderings.
///
/// This does not become redundant when the tag grammar is validated upstream:
/// a rejected tag's own rejection message still has to name it.
#[test]
fn a_foreign_tag_is_escaped_into_the_failure_message_that_names_it() {
    let forged = tag_failure(
        "1.2.3\n[2026-08-14 INFO] copied 121 of 121 packages ok",
        CopyError::PushRejected("507 insufficient storage".to_string()),
    )
    .expect("an aggregating failure must not abort the run");
    assert!(
        !forged.contains('\n') && forged.contains("\\n"),
        "the newline must be escaped, not echoed: {forged:?}"
    );

    let spoofed = tag_failure(
        "1.2.3\u{202e}",
        CopyError::SourceUnavailable("connection reset".to_string()),
    )
    .expect("an aggregating failure must not abort the run");
    assert!(
        !spoofed.contains('\u{202e}') && spoofed.contains("\\u{202e}"),
        "the direction override must be escaped: {spoofed:?}"
    );

    // Escaped, still identifiable — an operator has to be able to tell which
    // tag failed, so this must not degrade into dropping the value.
    assert!(spoofed.contains("1.2.3"), "the tag itself must survive: {spoofed:?}");
}

/// The same escape on the success path, which no failure test reaches.
///
/// `record_tag`'s `tracing::info!` names both the catalog key and the tag, and
/// a run that copies cleanly is exactly the run whose log an operator skims
/// rather than reads — the best place to forge a line. `tracing` has no capture
/// seam in this crate, so this is asserted on the format string itself, with
/// comments stripped first: the doc comment above the function names the
/// convention, and an unstripped scan would match that instead of the code.
#[test]
fn the_copy_log_line_escapes_both_the_catalog_key_and_the_tag() {
    let body = function_body("record_tag");
    assert!(
        body.contains("copied {package:?} tag {:?}:"),
        "both foreign strings must be {{:?}}-formatted in the success log line"
    );
}

// ── C-040 — the write-side classifier ───────────────────────────────────────

#[test]
fn a_refusal_of_upstream_bytes_fails_only_its_package() {
    // `index_write` chooses `ExecutionFailed` for exactly this: a root that is
    // not an object, a `content` pointer that is not an image index. Aborting
    // would let one malformed upstream package deny a whole-catalog mirror.
    let message = write_failure(MirrorError::ExecutionFailed(vec![
        "root document is not a JSON object".to_string(),
        "and a second reason".to_string(),
    ]))
    .expect("a refusal of upstream bytes must not abort the run");
    assert_eq!(message, "root document is not a JSON object; and a second reason");
}

#[test]
fn every_other_write_failure_aborts_the_run() {
    // Table-driven over every variant a write path can produce, so a new one
    // is classified deliberately rather than inheriting a fall-through.
    let aborting = vec![
        MirrorError::IndexWriteError("no space left on device".to_string()),
        MirrorError::IndexFormatUnsupported(2),
        MirrorError::SourceError("index fetch failed".to_string()),
        MirrorError::TargetError("destination probe did not answer".to_string()),
        MirrorError::SpecInvalid(vec!["bad key".to_string()]),
    ];
    for error in aborting {
        let expected = format!("{error:?}");
        let propagated = write_failure(error).expect_err("a local failure must abort the run");
        assert_eq!(
            format!("{propagated:?}"),
            expected,
            "propagated verbatim, never re-wrapped"
        );
    }
}

// ── The report rows and counters (C-042) ────────────────────────────────────

#[test]
fn a_clean_pass_reports_copied_and_a_failed_one_carries_every_message() {
    assert!(matches!(finish(Vec::new()), PackageStep::Copied));

    let PackageStep::Failed(detail) = finish(vec!["tag 'a': gone".to_string(), "tag 'b': 429".to_string()]) else {
        panic!("a pass with failures is not Copied");
    };
    assert_eq!(detail, "tag 'a': gone; tag 'b': 429");
}

#[test]
fn a_report_row_carries_a_detail_only_for_a_failure() {
    assert_eq!(package_report("ns/pkg", PackageStep::Copied).detail, None);
    assert_eq!(package_report("ns/pkg", PackageStep::Skipped).detail, None);

    let failed = package_report("ns/pkg", PackageStep::Failed("tag 'a': gone".to_string()));
    assert_eq!(failed.outcome, PackageOutcome::Failed);
    assert_eq!(failed.detail.as_deref(), Some("tag 'a': gone"));
    assert_eq!(failed.name, "ns/pkg");
}

#[test]
fn counters_tally_every_row_across_every_source() {
    let sources = vec![
        source_report(
            "upstream",
            false,
            vec![
                row("a/one", PackageOutcome::Copied),
                row("a/two", PackageOutcome::Skipped),
                row("a/three", PackageOutcome::Failed),
            ],
        ),
        source_report("other", false, vec![row("b/one", PackageOutcome::Copied)]),
    ];

    let counters = counters(&sources);
    assert_eq!(
        counters,
        RunCounters {
            total: 4,
            copied: 2,
            skipped: 1,
            failed: 1,
        }
    );
    assert_eq!(counters.total, counters.copied + counters.skipped + counters.failed);
}

#[test]
fn a_short_circuited_source_contributes_no_rows_and_no_counts() {
    // S-002: the run is a no-op but must not be silent. The counters are all
    // zero and the per-source line in the plain rendering is what says why.
    let sources = vec![source_report("upstream", true, Vec::new())];
    assert_eq!(counters(&sources), RunCounters::default());
    assert!(sources[0].short_circuited);
}

// ── C-047 — what gets into the confirmed set ────────────────────────────────

fn tag_plan(tag: &str, seed: &[u8]) -> registry_copy::TagCopyPlan {
    registry_copy::TagCopyPlan {
        tag: tag.to_string(),
        content: ocx_lib::oci::Algorithm::Sha256.hash(seed),
    }
}

#[test]
fn a_tag_joins_the_confirmed_set_only_when_its_content_landed() {
    let entry = tag_plan("1.2.3", b"index bytes");
    let mut confirmed = BTreeSet::new();
    let mut objects = BTreeMap::new();
    let mut failures = Vec::new();

    record_tag(
        "ns/pkg",
        &entry,
        Ok((registry_copy::CopyStats::default(), b"index bytes".to_vec())),
        &mut confirmed,
        &mut objects,
        &mut failures,
    )
    .expect("a successful copy does not abort");

    assert!(confirmed.contains("1.2.3"), "a copied tag must be confirmed");
    assert_eq!(
        objects.get(&entry.content).map(Vec::as_slice),
        Some(b"index bytes".as_slice()),
        "the verified bytes become the dispatch object, from the same fetch"
    );
    assert!(failures.is_empty());
}

#[test]
fn a_failed_tag_never_reaches_the_confirmed_set() {
    // The defect this guards: confirming a tag whose content copy failed
    // publishes a root pointing at content that is not at the destination, and
    // C-047's union then makes that pointer permanent.
    let entry = tag_plan("1.2.3", b"index bytes");

    for error in [
        CopyError::ContentMissing {
            content: entry.content.clone(),
        },
        CopyError::PushRejected("507 insufficient storage".to_string()),
        CopyError::DigestMismatch {
            expected: entry.content.clone(),
            actual: ocx_lib::oci::Algorithm::Sha256.hash(b"tampered"),
        },
    ] {
        let mut confirmed = BTreeSet::new();
        let mut objects = BTreeMap::new();
        let mut failures = Vec::new();

        record_tag(
            "ns/pkg",
            &entry,
            Err(error),
            &mut confirmed,
            &mut objects,
            &mut failures,
        )
        .expect("an aggregating failure does not abort the run");

        assert!(confirmed.is_empty(), "a failed tag must not be confirmed");
        assert!(objects.is_empty(), "a failed tag contributes no dispatch object");
        assert_eq!(failures.len(), 1, "the failure is counted and reported");
        assert!(failures[0].starts_with("tag \"1.2.3\": "));
    }
}

#[test]
fn an_abort_during_a_tag_copy_stops_the_run_without_recording_anything() {
    let entry = tag_plan("1.2.3", b"index bytes");
    let mut confirmed = BTreeSet::new();
    let mut objects = BTreeMap::new();
    let mut failures = Vec::new();

    let error = record_tag(
        "ns/pkg",
        &entry,
        Err(CopyError::Abort(MirrorError::TargetError(
            "destination answered 401".to_string(),
        ))),
        &mut confirmed,
        &mut objects,
        &mut failures,
    )
    .expect_err("a non-authoritative destination read aborts the run");

    assert!(matches!(error, MirrorError::TargetError(_)));
    assert!(confirmed.is_empty() && objects.is_empty() && failures.is_empty());
}

#[test]
fn two_tags_sharing_a_dispatch_object_write_it_once() {
    let content = ocx_lib::oci::Algorithm::Sha256.hash(b"shared index");
    let mut confirmed = BTreeSet::new();
    let mut objects = BTreeMap::new();
    let mut failures = Vec::new();

    for tag in ["1.2.3", "1.2"] {
        record_tag(
            "ns/pkg",
            &registry_copy::TagCopyPlan {
                tag: tag.to_string(),
                content: content.clone(),
            },
            Ok((registry_copy::CopyStats::default(), b"shared index".to_vec())),
            &mut confirmed,
            &mut objects,
            &mut failures,
        )
        .expect("no abort");
    }

    assert_eq!(confirmed.len(), 2, "a cascade tag is a tag like any other");
    assert_eq!(objects.len(), 1, "the dispatch object is keyed by digest");
}

// ── Structural guards: the call order IS the contract ───────────────────────

#[test]
fn the_stripped_source_really_drops_the_comments_that_name_these_calls() {
    // The guard on the guards. `registry_sync.rs`'s doc comments name
    // `read_root`, `from_env` and the ordering below; without stripping, every
    // assertion in this section matches its own documentation.
    let raw = include_str!("../registry_sync.rs");
    let stripped = module_source_without_comments();
    assert!(
        raw.contains("`read_root`"),
        "the doc comment naming the forbidden call has moved — re-point this guard"
    );
    assert!(
        !stripped.contains("`read_root`"),
        "comment stripping is what makes the denylists below mean anything"
    );
}

#[test]
fn phase_zero_creates_the_output_tree_before_deriving_any_cache_path() {
    let body = function_body("bootstrap");
    assert_ordered(&body, "create_dir_all", "cache::locks_dir");
    assert_ordered(&body, "create_dir_all", "build_index_store");
}

#[test]
fn collision_detection_runs_over_every_source_before_the_copy_phase() {
    // C-015 is a whole-run property. Folded into the per-source loop, source 1
    // is fully mirrored — hours, gigabytes, roots written — before source 2's
    // expansion reveals the clash.
    let body = function_body("execute_registry_sync");
    assert_ordered(&body, "prepare_source", "detect_collisions");
    assert_ordered(&body, "detect_collisions", "copy_sources");
}

#[test]
fn the_package_write_follows_c030s_order_exactly() {
    let body = function_body("write_package");
    // `root exists ⇒ its dispatch objects exist`. Reversed, a root that lands
    // while a dispatch write does not is skipped forever and `--repair-catalog`
    // cannot help, because `regenerate_catalog` only writes `c/index.json`.
    assert_ordered(&body, "write_dispatch_objects", "merge_root_tags");
    assert_ordered(&body, "merge_root_tags", "rewrite_root");
    assert_ordered(&body, "rewrite_root", "begin_catalog_transaction");
    // Qualified: `rewrite_root` *contains* `write_root`, so the bare needle
    // matches the line above and the assertion inverts itself.
    assert_ordered(&body, "begin_catalog_transaction", "index_write::write_root(");
    assert_ordered(&body, "index_write::write_root(", "transaction.commit");
    // After the commit, never before: `write_config_json` re-acquires the same
    // source lock rather than inheriting the transaction's, so the inverted
    // order self-deadlocks for the whole lock timeout.
    assert_ordered(&body, "transaction.commit", "write_config_json");
}

#[test]
fn the_merge_is_handed_the_confirmed_set_and_never_the_sources_tags() {
    // C-047, and the one defect every `index_write` unit test still passes
    // through: hand `source_root.tags` over instead of `confirmed` and a
    // partial run publishes tags whose content was never copied — while a
    // second run's merge then makes the wrong set permanent.
    let body = function_body("write_package");
    assert!(
        body.contains("merge_root_tags(&source_value, destination_root, confirmed)"),
        "the merge must be handed the confirmed set verbatim"
    );
    assert!(
        !function_body("sync_package").contains("merge_root_tags"),
        "the merge belongs to write_package, which only ever sees `confirmed`"
    );
}

#[test]
fn the_local_root_is_read_uncatalogued_and_a_read_error_is_not_an_absent_root() {
    let body = function_body("sync_package");
    assert!(
        body.contains("read_root_uncatalogued("),
        "the local root must be read uncatalogued"
    );
    assert!(
        !body.contains("store.read_root("),
        "`read_root` opens its own catalog transaction on a straddle and self-deadlocks against this function's"
    );
    // `Ok(None)` and `Err` must not collapse: mapping a failed read to `None`
    // truncates `tags{}` on a corrupt root, which is the deletion C-047 exists
    // to prevent.
    assert!(
        body.contains("Err(error) => {") && body.contains("cannot read the mirrored root document"),
        "a read error must fail its package, not degrade to a first publish"
    );
    assert!(
        !body.contains(".unwrap_or(None)") && !body.contains(".unwrap_or_default()"),
        "a read error must never be swallowed into an absent root"
    );
}

#[test]
fn the_source_read_seam_is_ssrf_guarded_and_never_mirror_rewritten() {
    let body = function_body("source_read_seam");
    assert!(
        body.contains(".ssrf_guard("),
        "the source registry is dialled at a host that came off a foreign index"
    );
    // ocx v0.6.0 deleted `ClientBuilder::from_env`, so the old needle here
    // could no longer match anything — a denylist that cannot fire reads as a
    // passing guard forever (TEST-11). The hazard moved call sites rather than
    // going away: the OCX_MIRRORS map now arrives through `registry_client`,
    // which installs it with `.mirrors(MirrorMap::new(…))`. Deny all three
    // spellings, because the realistic regression is additive — someone adds
    // `.mirrors(…)` beside the surviving `.ssrf_guard(…)`, and the positive
    // assertion above stays green.
    for forbidden in [".mirrors(", "MirrorMap", "registry_client"] {
        assert!(
            !body.contains(forbidden),
            "`{forbidden}` installs the OCX_MIRRORS map, which would dial a host validate_root_host never approved"
        );
    }
}

#[test]
fn the_mirror_rewrite_denylist_names_a_call_that_still_exists() {
    // The guard on the guard above. Its needles are denials, so they pass
    // vacuously once the spelling they name is gone — which is exactly what
    // happened to `from_env` at the ocx v0.6.0 bump. Pin them to a call that
    // is real *today*: `registry_client` must still be the mirror-installing
    // constructor, and it must still install the map with `.mirrors(`.
    let client_source = include_str!("../../command/package/mod.rs");
    assert!(
        client_source.contains("fn registry_client("),
        "registry_client has been renamed or removed — re-point the denylist in \
         the_source_read_seam_is_ssrf_guarded_and_never_mirror_rewritten"
    );
    assert!(
        client_source.contains(".mirrors("),
        "registry_client no longer installs the OCX_MIRRORS map with `.mirrors(` — \
         find where it moved and re-point the denylist"
    );
}

#[test]
fn the_cache_digest_is_recorded_only_on_a_fully_successful_non_dry_run() {
    // C-039: the recorded digest means "a fully successful run mirrored this
    // exact catalog". Recorded after a partial run, the next run short-circuits
    // past work that never landed.
    let body = function_body("copy_sources");
    assert!(
        body.contains("if !source_failed && !options.dry_run {"),
        "record_digest must be guarded by both a clean source pass and a real run"
    );
    assert_ordered(&body, "if !source_failed && !options.dry_run {", "record_digest");
}

#[test]
fn the_blob_pool_is_run_scoped_and_the_package_width_is_the_operators_knob() {
    // C-026's ceiling is `max_blobs × largest_blob`, and it holds because ONE
    // pool covers the whole run — which is also why packages may be copied
    // concurrently without moving it. What must not appear is a second pool
    // per source or per package.
    let body = function_body("copy_sources");
    assert!(
        body.contains("let blob_semaphore = Arc::new(Semaphore::new(spec.concurrency.max_blobs));"),
        "the pool is constructed once, outside every loop"
    );
    assert!(
        body.contains("blob_semaphore: Arc::clone(&blob_semaphore)"),
        "each source's context shares that one pool"
    );
    // The width is the operator's, and `fail_fast` overrides it: that flag
    // promises nothing after the first failure is copied, which only a
    // sequential path can keep.
    assert!(
        body.contains("let width = if fail_fast { 1 } else { spec.concurrency.max_packages };"),
        "fail_fast forces width 1"
    );
    // `buffer_unordered`, not `buffered`: a finished-but-unyielded skip must not
    // pin a head slot and collapse the effective width (the incremental-sweep
    // workload `max_packages` exists to speed up). The catalog order the report
    // depends on is restored by the sort, not by the stream combinator.
    assert!(
        body.contains(".buffer_unordered(width)"),
        "the concurrent path yields in completion order under buffer_unordered"
    );
    assert!(
        body.contains("outcomes.sort_by_key(|row| row.position);"),
        "completion order is re-sorted into catalog order before the report is built"
    );
    assert!(
        !body.contains("JoinSet") && !body.contains("tokio::spawn"),
        "the packages stay on one task: the copy's shared state is guarded for concurrent futures, not for a spawned one"
    );
}

#[test]
fn the_source_client_is_seeded_for_the_host_the_pointer_names() {
    // `build_source_client` can only seed the source's *logical* `registry:`
    // (`ocx.sh`); every request the source client makes goes to the physical
    // host the root's pointer names (`ghcr.io`), and the fork's token cache
    // keys on that host. A miss is silent — the request goes out with no
    // `Authorization` header at all, which GHCR answers 401, so before this
    // call the public index was uncopyable from the first referrer query on.
    let body = function_body("sync_package");
    assert!(
        body.contains("registry_copy::ensure_source_auth(context, &source_registry, &credentialed_hosts).await;"),
        "the physical host's credential is seeded"
    );
    // The host seeded must be the one `parse_physical_repository` read off the
    // root, not the source's logical `registry:` — rebinding it to the latter
    // is exactly the bug this fixed, and it would leave the needle above
    // matching, so pin the provenance too.
    assert_ordered(&body, "parse_physical_repository", "ensure_source_auth");
    // The credential is resolved only for a host the operator named — the
    // source's `registry:` plus its `trusted_hosts` — so a lookalike physical
    // host from a hostile root cannot slug-collide onto a real credential.
    assert!(
        body.contains("source.registry.as_str()") && body.contains("source.trusted_hosts.iter()"),
        "the credentialed-host allow-list is the source's own registry and trusted_hosts"
    );
    assert_ordered(
        &body,
        "ensure_source_auth",
        "for entry in registry_copy::tag_copy_plan(&source_root.tags) {",
    );
}

// ── Behavioural cover for the structural guards above ───────────────────────
//
// `write_package` takes a store and byte slices and touches no network, so the
// whole publish sequence runs against a tempdir. The guards above assert on the
// source text because that is the only tool for a call a unit test cannot
// reach; this child asserts on the *outcome* of the calls that can be reached,
// which is what survives a reformat and what catches a merge handed the right
// set that still gets the union wrong.
#[path = "tests/publish.rs"]
mod publish;

// The repair phase is reachable the same way and for the same reason: it takes
// a spec, an options struct and a store, and touches no network — so what
// `--repair-catalog` writes, and the two cases where it must write nothing, are
// asserted against a real tree rather than against the source text.
#[path = "tests/repair.rs"]
mod repair;
