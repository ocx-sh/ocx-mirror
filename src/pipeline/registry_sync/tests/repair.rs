// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! [`repair_catalogs`] against a real store — what `--repair-catalog` writes,
//! and the two cases where it must write nothing.
//!
//! Behavioural throughout, because it can be: the repair phase takes a spec, an
//! options struct and a store, and touches no network at all. Both negative
//! cases are asserted on the **filesystem** rather than on a return value —
//! `regenerate_catalog` reports what it changed, so a run that wrongly repaired
//! a tree returns `Ok` and looks identical from the outside.
//!
//! Every test here pairs its negative half with a positive one in the same
//! function: the tree is snapshotted, the run that must change nothing is
//! asserted byte-identical, and then the same fixture is run through the branch
//! that *does* repair — so "nothing changed" can never pass because there was
//! nothing to change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ocx_lib::file_structure::IndexStore;
use serde_json::{Value, json};

use super::super::*;
use super::options;

/// The source subtree these tests publish into — `output: <dir>/public` +
/// `as: ocx.sh` ⇒ `<dir>/public/ocx.sh/{c/,p/}`.
const AS_NAME: &str = "ocx.sh";
/// A source the spec names but that nothing has ever mirrored, so its subtree
/// under `output:` does not exist.
const ABSENT: &str = "ghcr.io";

/// A store over `<dir>/public` with its locks redirected out of the served
/// tree, and the output root the snapshots are taken over.
async fn store_at(directory: &Path) -> (IndexStore, PathBuf) {
    let output = directory.join("public");
    tokio::fs::create_dir_all(&output)
        .await
        .expect("create the output tree");
    (
        index_write::build_index_store(&output, &directory.join("locks")),
        output,
    )
}

/// A spec whose `output:` is `output` and whose sources are `as_names`, in
/// order.
fn spec_with_sources(output: &Path, as_names: &[&str]) -> RegistrySpec {
    let sources: String = as_names
        .iter()
        .enumerate()
        .map(|(index, as_name)| {
            format!("  - registry: localhost:500{index}\n    index: http://localhost:808{index}\n    as: {as_name}\n")
        })
        .collect();
    let document = format!(
        r#"
target:
  registry: localhost:5002
  repository: mirror
output: {}
destination: "{{registry}}/{{namespace}}/{{package}}"
sources:
{sources}"#,
        output.display()
    );
    serde_yaml_ng::from_str(&document).expect("the test spec parses")
}

/// `--repair-catalog` on, `--dry-run` as given.
fn repair_options(dry_run: bool) -> RegistrySyncOptions {
    let mut options = options();
    options.repair_catalog = true;
    options.dry_run = dry_run;
    options
}

/// Publish one root document **and** its derived catalog entry — the consistent
/// state an ordinary sync leaves behind.
async fn seed_package(store: &IndexStore, as_name: &str, package: &str) {
    let bytes = serialize_root(&json!({
        "repository": format!("oci://registry.test/mirror/{as_name}/{package}"),
        "tags": {},
    }));
    let mut transaction = store
        .begin_catalog_transaction(as_name)
        .await
        .expect("open the catalog transaction");
    index_write::write_root(&mut transaction, package, &bytes)
        .await
        .expect("write the root document");
    transaction.commit().await.expect("commit the catalog");
}

/// The drift `--repair-catalog` exists for: a catalog entry naming a package
/// whose root document is gone. Nothing else can clear it — `write_root` only
/// upserts.
///
/// Two packages are seeded and one root removed on purpose: a derivation that
/// found **no** root at all is refused outright by `regenerate_catalog` rather
/// than replacing a live catalog with an empty one, so a one-package fixture
/// would exercise that guard instead of the repair.
async fn seed_drifted_source(store: &IndexStore, as_name: &str) {
    seed_package(store, as_name, "kitware/cmake").await;
    seed_package(store, as_name, "ns/ghost").await;
    std::fs::remove_file(store.root_document_path(as_name, "ns/ghost")).expect("remove the ghost's root document");
    assert!(
        catalog_packages(store, as_name).contains_key("ns/ghost"),
        "fixture: the catalog must still name the package whose root was removed"
    );
}

/// `c/index.json`'s `packages` map for one source, straight off disk.
fn catalog_packages(store: &IndexStore, as_name: &str) -> serde_json::Map<String, Value> {
    let raw = std::fs::read(store.source_catalog_path(as_name)).expect("read c/index.json");
    let document: Value = serde_json::from_slice(&raw).expect("parse c/index.json");
    document["packages"].as_object().expect("packages is an object").clone()
}

/// Every regular file under `root`, with its bytes.
///
/// The whole tree, never a name allow-list: `regenerate_catalog` inherits side
/// effects from the primitives it drives (a `c/index.json.etag` removal, a
/// `create_dir_all` inside `lock_source`), and a `c/index.json`-only assertion
/// would miss every one of them.
fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(directory) = queue.pop() {
        for entry in std::fs::read_dir(&directory).expect("read directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                queue.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).expect("read file");
            snapshot.insert(path, bytes);
        }
    }
    snapshot
}

/// **C-043 / S-019.** `--dry-run --repair-catalog` writes nothing.
///
/// `--dry-run`'s own help text promises a run that transfers and writes
/// nothing, and `regenerate_catalog` is the most destructive write this binary
/// makes into a served tree: it replaces `c/index.json` wholesale, dropping
/// every entry whose root the tree lacks. Against an operator's committed
/// checkout that is a data-losing write behind a flag that says it makes none.
///
/// The second half runs the identical fixture without `--dry-run`, so the
/// byte-identity assertion cannot pass merely because the tree had no drift.
#[tokio::test]
async fn a_dry_run_repairs_no_catalog_and_leaves_the_served_tree_byte_identical() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, output) = store_at(directory.path()).await;
    seed_drifted_source(&store, AS_NAME).await;
    let spec = spec_with_sources(&output, &[AS_NAME]);
    let before = tree_snapshot(&output);

    repair_catalogs(&spec, &repair_options(true), &store)
        .await
        .expect("a dry run cannot fail: it does nothing");

    assert_eq!(
        tree_snapshot(&output),
        before,
        "--dry-run must leave every byte of the served tree alone"
    );
    assert!(
        catalog_packages(&store, AS_NAME).contains_key("ns/ghost"),
        "--dry-run must not drop the entry a real repair would drop"
    );

    // The same fixture through the branch that does repair — without this, the
    // assertions above would pass on a tree that simply had nothing to repair.
    repair_catalogs(&spec, &repair_options(false), &store)
        .await
        .expect("the repair");

    assert!(
        !catalog_packages(&store, AS_NAME).contains_key("ns/ghost"),
        "a real --repair-catalog run drops the entry whose root is gone"
    );
}

/// A source whose subtree does not exist yet is **skipped**, and the sources
/// after it are still repaired.
///
/// `bootstrap` creates `output:` and nothing under it, so an absent subtree is
/// the ordinary state of a first run and of every run that adds a source — not
/// a fault. `regenerate_catalog` refuses it (exit 74), which would abort the
/// whole run having repaired nothing, including the sources that were fine.
/// The absent source is listed **first** precisely so that the repair of the
/// second one proves the run continued rather than stopping.
#[tokio::test]
async fn an_absent_source_subtree_is_skipped_and_the_sources_after_it_are_still_repaired() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, output) = store_at(directory.path()).await;
    seed_drifted_source(&store, AS_NAME).await;
    let absent_subtree = output.join(ABSENT);
    assert!(
        !absent_subtree.exists(),
        "fixture: the first source's subtree must be missing before the run"
    );

    repair_catalogs(
        &spec_with_sources(&output, &[ABSENT, AS_NAME]),
        &repair_options(false),
        &store,
    )
    .await
    .expect("a subtree that does not exist yet has nothing to repair, and is not a run failure");

    assert!(
        !catalog_packages(&store, AS_NAME).contains_key("ns/ghost"),
        "the source that does exist must still have been repaired"
    );
    assert!(
        !absent_subtree.exists(),
        "skipping must not materialize a subtree — an empty directory beside a served checkout \
         is exactly what the upstream refusal exists to prevent"
    );
}

/// The tolerance is scoped to the absence and nothing else: a root under `p/`
/// that will not parse still aborts the run.
///
/// Widening the skip to "any repair failure" would silently accept a corrupt
/// tree under the flag whose job is to repair it.
#[tokio::test]
async fn a_repair_that_fails_for_any_other_reason_still_aborts_the_run() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, output) = store_at(directory.path()).await;
    seed_drifted_source(&store, AS_NAME).await;
    std::fs::write(
        store.root_document_path(AS_NAME, "kitware/cmake"),
        b"{ this is not a root document",
    )
    .expect("corrupt the root document");

    let error = repair_catalogs(&spec_with_sources(&output, &[AS_NAME]), &repair_options(false), &store)
        .await
        .expect_err("an unparseable root is a real failure, not an absent subtree");

    assert!(
        matches!(error, MirrorError::IndexWriteError(_)),
        "a repair failure aborts the run as IndexWriteError (74), got {error:?}"
    );
}
