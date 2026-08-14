// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-027, C-029…C-031, C-033, C-034, C-036 — everything that touches the tree
//! under `output:`.

use serde_json::json;

use super::super::*;
use super::support::*;

/// Write one package the way C-030 orders it: dispatch objects, then the root
/// inside a per-package transaction, then `commit`, then `config.json`.
async fn publish(store: &IndexStore, objects: &[(Digest, Vec<u8>)], root: &[u8]) -> Result<(), MirrorError> {
    write_dispatch_objects(store, AS_NAME, PACKAGE, objects).await?;
    let rewritten = rewrite_root(root, POINTER)?;
    let mut transaction = store
        .begin_catalog_transaction(AS_NAME)
        .await
        .expect("begin catalog transaction");
    write_root(&mut transaction, PACKAGE, &rewritten).await?;
    transaction.commit().await.expect("commit");
    write_config_json(store, AS_NAME).await
}

#[tokio::test]
async fn build_index_store_keeps_locks_out_of_the_served_tree() {
    // Forgetting `with_locks_root` is silent: `IndexStore::new` defaults it to
    // `root/locks`, inside the tree the operator serves and commits.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());

    assert_eq!(store.root(), output);
    assert_eq!(store.locks_root(), directory.path().join("locks"));
    assert!(!store.locks_root().starts_with(&output));
}

#[tokio::test]
async fn write_root_refuses_a_pointer_that_does_not_parse() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());
    let source = root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &digest("a"))]);

    // RED — a rewrite that produced an unparseable physical reference must
    // fail at the write rather than shipping. A `|_| Ok(())` hook would let it
    // through.
    let broken = rewrite_root(&source, "ghcr.io/ocx-sh/cmake").expect("rewrite");
    let mut transaction = store.begin_catalog_transaction(AS_NAME).await.expect("begin");
    let error = write_root(&mut transaction, PACKAGE, &broken)
        .await
        .expect_err("a pointer with no oci:// scheme must be refused");
    assert!(
        matches!(error, MirrorError::IndexWriteError(ref message) if message.contains(PACKAGE)),
        "{error:?}"
    );
    drop(transaction);

    // GREEN — the same write with a well-formed pointer lands.
    let good = rewrite_root(&source, POINTER).expect("rewrite");
    let mut transaction = store.begin_catalog_transaction(AS_NAME).await.expect("begin");
    write_root(&mut transaction, PACKAGE, &good).await.expect("write root");
    transaction.commit().await.expect("commit");

    assert!(store.root_document_path(AS_NAME, PACKAGE).exists());
}

#[tokio::test]
async fn a_bare_image_manifest_is_refused_as_a_dispatch_object() {
    // `o/` is indices-only by format invariant and `write_dispatch_object`
    // verifies the digest but NOT the shape, so a source serving a leaf
    // manifest under a `content` pointer would otherwise poison the tree with
    // a document no reader accepts. The upstream registry is in scope.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());

    // RED — a leaf manifest.
    let (manifest_digest, manifest) = image_manifest();
    let error = write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(manifest_digest.clone(), manifest)])
        .await
        .expect_err("a bare image manifest must be refused");
    assert!(refusal_message(&error).contains("not an OCI image index"), "{error:?}");
    assert!(
        !store.dispatch_object_path(AS_NAME, PACKAGE, &manifest_digest).exists(),
        "the refused document must not have been written"
    );

    // GREEN — a genuine image index of the same size class lands.
    let (index_digest, index) = image_index("child");
    write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(index_digest.clone(), index)])
        .await
        .expect("an image index is accepted");
    assert!(store.dispatch_object_path(AS_NAME, PACKAGE, &index_digest).exists());
}

#[tokio::test]
async fn a_semantically_invalid_image_index_is_refused() {
    // Deserialization proves shape only — `schemaVersion` is an unconstrained
    // integer, so `{"schemaVersion":1,"manifests":[]}` parses happily.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());
    let bytes = serde_json::to_vec(&json!({ "schemaVersion": 1, "manifests": [] })).expect("serialize");
    let claimed = ocx_lib::oci::Algorithm::Sha256.hash(&bytes);

    let error = write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(claimed, bytes)])
        .await
        .expect_err("schemaVersion 1 is not a valid image index");

    assert!(refusal_message(&error).contains("schemaVersion"), "{error:?}");
}

#[tokio::test]
async fn a_dispatch_object_whose_digest_does_not_match_is_refused() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());
    let (_correct, bytes) = image_index("child");

    let error = write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(digest("a lie"), bytes)])
        .await
        .expect_err("the store re-verifies the digest itself");

    assert!(matches!(error, MirrorError::IndexWriteError(_)), "{error:?}");
}

#[tokio::test]
async fn a_refused_dispatch_object_leaves_no_root_behind() {
    // The C-030 ordering invariant, observed through its consequence: written
    // BEFORE the root, a dispatch failure means the root never lands, so the
    // package is simply absent and the next ordinary run re-copies it. Written
    // after, the root would satisfy all three skip conditions forever.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());
    let (manifest_digest, manifest) = image_manifest();
    let root = root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &digest("a"))]);

    let error = publish(&store, &[(manifest_digest, manifest)], &root)
        .await
        .expect_err("the dispatch write fails first");

    assert!(refusal_message(&error).contains("not an OCI image index"), "{error:?}");
    assert!(
        !store.root_document_path(AS_NAME, PACKAGE).exists(),
        "a root must never outlive a failed dispatch write"
    );
}

#[tokio::test]
async fn the_two_error_classes_carry_the_exit_codes_c040_assigns_them() {
    // The distinction is by WHOSE FAULT it is, not by which function failed.
    // Collapsing the two would let one upstream package publishing a bare
    // manifest deny a whole 121-package run.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());

    // Foreign data that will not validate → aggregating, exit 1.
    let (manifest_digest, manifest) = image_manifest();
    let upstream = write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(manifest_digest, manifest)])
        .await
        .expect_err("a bare image manifest is refused");
    assert_eq!(
        upstream.kind_exit_code(),
        ocx_lib::cli::ExitCode::Failure,
        "{upstream:?}"
    );

    // A local filesystem write that cannot succeed → whole-run abort, exit 74.
    // A plain file where the source subtree must be is a tree that cannot be
    // written, and continuing to write it is meaningless.
    std::fs::create_dir_all(&output).expect("create output");
    std::fs::write(output.join(AS_NAME), b"not a directory").expect("block the subtree");
    let local = write_config_json(&store, AS_NAME)
        .await
        .expect_err("the source subtree cannot be created");
    assert!(matches!(local, MirrorError::IndexWriteError(_)), "{local:?}");
    assert_eq!(local.kind_exit_code(), ocx_lib::cli::ExitCode::IoError, "{local:?}");
}

#[tokio::test]
async fn config_json_is_written_when_absent_and_left_alone_when_present() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());
    let target = store.source_config_path(AS_NAME);
    let authored = br#"{"format_version": 1, "name_segments": 2}"#;

    // The operator's own file, placed on disk directly — the only way it can
    // arrive, now that the function synthesizes its content and accepts no
    // bytes from any caller.
    std::fs::create_dir_all(target.parent().expect("config.json has a parent")).expect("create source subtree");
    std::fs::write(&target, authored).expect("author config.json");

    #[cfg(unix)]
    let inode_before = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&target).expect("stat").ino()
    };

    // A sync over that tree. What the function would synthesize differs from
    // what is there (no `name_segments`), so an unconditional atomic rename
    // would clobber the operator's declaration and churn the mtime of a file
    // in a tree people commit.
    write_config_json(&store, AS_NAME).await.expect("write config.json");

    assert_eq!(
        std::fs::read(&target).expect("read config.json"),
        authored,
        "an existing config.json must be left byte-identical"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&target).expect("stat").ino(),
            inode_before,
            "the file was replaced by a rename, so its mtime churned too"
        );
    }
}

#[tokio::test]
async fn config_json_is_always_synthesized_never_copied_from_the_source() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());

    write_config_json(&store, AS_NAME).await.expect("write config.json");

    let written = std::fs::read(store.source_config_path(AS_NAME)).expect("read config.json");
    assert_eq!(
        String::from_utf8(written).expect("UTF-8"),
        "{\n  \"format_version\": 1\n}\n",
        "`name_segments` is an operator declaration ocx refuses to guess, and a value \
         copied from a hostile upstream would change how the whole fleet resolves names \
         against this mirror"
    );
}

#[tokio::test]
async fn a_write_repair_write_cycle_is_byte_identical() {
    // At the pinned ocx `CatalogTransaction::commit` and `regenerate_catalog`
    // both emit through `serialize_catalog`, so the mirror does not need to
    // choose a writer. Without this assertion a future divergence would
    // surface as perpetual one-byte drift that reads like a mirror bug.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());
    let (index_digest, index) = image_index("child");
    let root = root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &digest("a"))]);

    publish(&store, &[(index_digest.clone(), index.clone())], &root)
        .await
        .expect("first publish");
    let catalog_path = store.source_catalog_path(AS_NAME);
    let root_path = store.root_document_path(AS_NAME, PACKAGE);
    let catalog_after_write = std::fs::read(&catalog_path).expect("read catalog");
    let root_after_write = std::fs::read(&root_path).expect("read root");

    let outcome = repair_catalog(&store, AS_NAME).await.expect("repair");
    assert_eq!(outcome.roots, 1);
    assert!(outcome.added.is_empty() && outcome.corrected.is_empty() && outcome.removed.is_empty());
    assert_eq!(
        std::fs::read(&catalog_path).expect("read catalog"),
        catalog_after_write,
        "repair rewrote the catalog it re-derived from unchanged bytes"
    );

    publish(&store, &[(index_digest, index)], &root)
        .await
        .expect("second publish");
    assert_eq!(std::fs::read(&catalog_path).expect("read catalog"), catalog_after_write);
    assert_eq!(std::fs::read(&root_path).expect("read root"), root_after_write);
}

#[tokio::test]
async fn repair_catalog_leaves_a_merged_root_untouched() {
    // `--repair-catalog` re-derives entries from the root bytes already on
    // disk and writes no other path, so it cannot undo C-047's union.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());
    let (index_digest, index) = image_index("child");
    let survivor = digest("run-one");
    let fresh = digest("run-two");

    let first = root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &survivor), ("latest", &survivor)],
    );
    publish(&store, &[(index_digest.clone(), index.clone())], &first)
        .await
        .expect("first publish");

    // A partial second run: the source moved both tags on, only `3.28.1`
    // copied.
    let destination = std::fs::read(store.root_document_path(AS_NAME, PACKAGE)).expect("read root");
    let source: serde_json::Value = serde_json::from_slice(&root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &fresh), ("latest", &fresh)],
    ))
    .expect("parse");
    let merged = merge_root_tags(&source, Some(&destination), &confirmed(&["3.28.1"])).expect("merge");
    publish(&store, &[(index_digest, index)], &serialize_root(&merged))
        .await
        .expect("second publish");

    let before_repair = std::fs::read(store.root_document_path(AS_NAME, PACKAGE)).expect("read root");
    repair_catalog(&store, AS_NAME).await.expect("repair");
    let after_repair = std::fs::read(store.root_document_path(AS_NAME, PACKAGE)).expect("read root");

    assert_eq!(before_repair, after_repair);
    assert_eq!(
        tag_digests(&after_repair),
        vec![
            ("3.28.1".to_string(), fresh.to_string()),
            ("latest".to_string(), survivor.to_string()),
        ],
        "repair must not undo the merge"
    );
}

#[tokio::test]
async fn nothing_but_wire_content_lands_under_output() {
    // C-036, asserted on the recursive listing rather than a name allow-list:
    // the served tree is a distributable artifact operators commit and rsync,
    // so a lock file or a cache file inside it travels with every copy.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());
    let (index_digest, index) = image_index("child");
    let root = root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &digest("a"))]);

    publish(&store, &[(index_digest, index)], &root).await.expect("publish");

    let listing = list_recursive(&output);
    assert_eq!(listing.first().map(String::as_str), Some(AS_NAME), "{listing:?}");
    for entry in &listing {
        let relative = entry.strip_prefix(&format!("{AS_NAME}/")).unwrap_or("");
        assert!(
            relative.is_empty()
                || relative == "config.json"
                || relative.starts_with("c/")
                || relative == "c"
                || relative.starts_with("p/")
                || relative == "p",
            "{entry} is neither config.json, c/ nor p/"
        );
        assert!(!entry.contains("locks"), "a lock landed in the served tree: {entry}");
        assert!(
            !entry.ends_with(".etag"),
            "a per-machine file landed in the served tree: {entry}"
        );
    }

    // …and the locks really were taken, somewhere else — otherwise the
    // assertion above is satisfied by a run that never locked anything.
    assert!(
        list_recursive(store.locks_root()).iter().any(|entry| !entry.is_empty()),
        "the lock root is empty, so the negative assertion above proves nothing"
    );
}
