// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-048 — the README/logo objects a root's `desc` names, written beside the
//! dispatch objects (ocx-mirror#69) and pruned once nothing names them
//! (ocx-mirror#71). Dispatch objects are tag history and are never touched.

use std::path::PathBuf;

use ocx_oci::Algorithm;

use super::super::*;
use super::support::*;

fn object(seed: &str, extension: &'static str) -> DescriptionObject {
    let bytes = format!("# {seed}\n").into_bytes();
    DescriptionObject {
        digest: Algorithm::Sha256.hash(&bytes),
        extension,
        bytes,
    }
}

/// `<output>/<as>/p/tools/cmake/o/sha256/<hex>.<ext>` — the path a catalog
/// renderer resolves a `desc` digest against, derived independently of the
/// writer so a drift in either shows.
fn object_path(output: &std::path::Path, object: &DescriptionObject) -> PathBuf {
    output
        .join(AS_NAME)
        .join("p")
        .join(PACKAGE)
        .join("o")
        .join("sha256")
        .join(format!("{}.{}", object.digest.hex(), object.extension))
}

#[tokio::test]
async fn the_objects_land_beside_the_dispatch_objects_under_their_own_extension() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());
    let (index_digest, index_bytes) = image_index("linux");
    let readme = object("readme", "md");
    let logo = object("logo", "svg");

    write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(index_digest.clone(), index_bytes)].into())
        .await
        .expect("dispatch object");
    write_description_objects(&store, AS_NAME, PACKAGE, &[readme.clone(), logo.clone()])
        .await
        .expect("description objects");

    assert_eq!(
        std::fs::read(object_path(&output, &readme)).expect("readme on disk"),
        readme.bytes
    );
    assert_eq!(
        std::fs::read(object_path(&output, &logo)).expect("logo on disk"),
        logo.bytes
    );
    let mut expected = vec![
        format!("{}.json", index_digest.hex()),
        format!("{}.svg", logo.digest.hex()),
        format!("{}.md", readme.digest.hex()),
    ];
    expected.sort();
    assert_eq!(
        list_recursive(&output.join(AS_NAME).join("p").join(PACKAGE).join("o").join("sha256")),
        expected,
        "one directory holds the image index and the objects the root's desc names"
    );
}

#[tokio::test]
async fn an_unchanged_object_is_not_rewritten() {
    // The served tree is one an operator commits: a rename of byte-identical
    // content churns the mtime and makes a scheduled sync a source of empty
    // commits (S-002).
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());
    let readme = object("readme", "md");
    let target = object_path(&output, &readme);

    write_description_objects(&store, AS_NAME, PACKAGE, std::slice::from_ref(&readme))
        .await
        .expect("first write");
    #[cfg(unix)]
    let inode_before = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&target).expect("stat").ino()
    };

    write_description_objects(&store, AS_NAME, PACKAGE, std::slice::from_ref(&readme))
        .await
        .expect("second write");

    assert_eq!(std::fs::read(&target).expect("read"), readme.bytes);
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
async fn the_prune_removes_what_the_root_no_longer_names_and_nothing_else() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());
    let (index_digest, index_bytes) = image_index("linux");
    let old_readme = object("old readme", "md");
    let new_readme = object("new readme", "md");
    let logo = object("logo", "png");

    write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(index_digest.clone(), index_bytes)].into())
        .await
        .expect("dispatch object");
    write_description_objects(&store, AS_NAME, PACKAGE, &[old_readme.clone(), logo.clone()])
        .await
        .expect("first description");
    // The description moved: a new README, the same logo.
    write_description_objects(&store, AS_NAME, PACKAGE, std::slice::from_ref(&new_readme))
        .await
        .expect("second description");

    prune_description_objects(
        &store,
        AS_NAME,
        PACKAGE,
        &[new_readme.digest.clone(), logo.digest.clone()],
    )
    .await
    .expect("prune");

    assert!(
        !object_path(&output, &old_readme).exists(),
        "the previous README is an orphan under D8"
    );
    assert!(
        object_path(&output, &new_readme).exists(),
        "the README the root names stays"
    );
    assert!(
        object_path(&output, &logo).exists(),
        "the logo the root still names stays"
    );
    assert!(
        store.dispatch_object_path(AS_NAME, PACKAGE, &index_digest).exists(),
        "dispatch objects are tag history and are never pruned (C-047)"
    );
}

#[tokio::test]
async fn the_prune_keeps_every_dispatch_object_even_when_the_root_names_no_description() {
    // A package that lost its description: `keep` is empty, and still nothing
    // under `.json` moves — the append-only tag ruling covers those.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());
    let (index_digest, index_bytes) = image_index("linux");
    let readme = object("readme", "md");

    write_dispatch_objects(&store, AS_NAME, PACKAGE, &[(index_digest.clone(), index_bytes)].into())
        .await
        .expect("dispatch object");
    write_description_objects(&store, AS_NAME, PACKAGE, std::slice::from_ref(&readme))
        .await
        .expect("description");

    prune_description_objects(&store, AS_NAME, PACKAGE, &[])
        .await
        .expect("prune");

    assert!(!object_path(&output, &readme).exists());
    assert!(store.dispatch_object_path(AS_NAME, PACKAGE, &index_digest).exists());
}

#[tokio::test]
async fn a_nested_package_inside_the_object_directory_is_left_alone() {
    // Catalog keys nest: `tools/cmake` and `tools/cmake/o/sha256/extra` are
    // both legal, and the second one's whole subtree lands inside the first
    // one's `o/sha256/`. The prune must step over that directory rather than
    // `remove_file` it — which is EISDIR, exit 74, and the whole run gone.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());
    let readme = object("readme", "md");
    write_description_objects(&store, AS_NAME, PACKAGE, std::slice::from_ref(&readme))
        .await
        .expect("description");
    let nested = format!("{PACKAGE}/o/sha256/extra");
    let (index_digest, index_bytes) = image_index("nested");
    write_dispatch_objects(&store, AS_NAME, &nested, &[(index_digest.clone(), index_bytes)].into())
        .await
        .expect("the nested package's dispatch object");
    let nested_root = output.join(AS_NAME).join("p").join(&nested).with_extension("json");
    std::fs::write(&nested_root, b"{}").expect("the nested package's root");

    prune_description_objects(&store, AS_NAME, PACKAGE, &[])
        .await
        .expect("prune");

    assert!(!object_path(&output, &readme).exists(), "the prune still ran");
    assert!(
        nested_root.exists(),
        "a sibling package's root is not this package's orphan"
    );
    assert!(
        store.dispatch_object_path(AS_NAME, &nested, &index_digest).exists(),
        "nor is anything under its subtree"
    );
}

#[tokio::test]
async fn a_package_with_no_object_directory_has_nothing_to_prune() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, _output) = store(directory.path());

    prune_description_objects(&store, AS_NAME, PACKAGE, &[])
        .await
        .expect("an absent `o/` is a package with nothing to prune, not an error");
}

#[tokio::test]
async fn a_stray_file_under_the_object_directory_is_skipped_not_fatal() {
    // `o/` holds one directory per digest algorithm; a stray file there is
    // nothing the mirror owns, and reading it as a directory would abort the
    // whole run (exit 74) over a `.DS_Store`.
    let directory = tempfile::tempdir().expect("temp dir");
    let (store, output) = store(directory.path());
    let readme = object("readme", "md");
    write_description_objects(&store, AS_NAME, PACKAGE, std::slice::from_ref(&readme))
        .await
        .expect("description");
    let stray = output.join(AS_NAME).join("p").join(PACKAGE).join("o").join(".DS_Store");
    std::fs::write(&stray, b"junk").expect("stray file");

    prune_description_objects(&store, AS_NAME, PACKAGE, &[])
        .await
        .expect("prune");

    assert!(stray.exists(), "not the mirror's to remove");
    assert!(!object_path(&output, &readme).exists(), "the prune still ran past it");
}
