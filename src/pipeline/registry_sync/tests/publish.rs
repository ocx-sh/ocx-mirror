// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! [`write_package`] driven against a real store — C-030's order and C-047's
//! `confirmed` set, proved by **outcome** rather than by call position.
//!
//! The sibling module asserts both on the source text, which is the right tool
//! for a call that cannot be reached without a registry. This one can:
//! `write_package` takes a store and byte slices and touches no network at all,
//! so the whole publish sequence runs on a tempdir. That buys three things a
//! structural guard cannot give:
//!
//! - it survives `cargo fmt` and any refactor that preserves behaviour, where a
//!   literal needle silently stops matching;
//! - it catches a merge that is handed the right set and still gets the union
//!   wrong, which no assertion about the *call* can see;
//! - it states the ordering as the invariant actually means it — *no root
//!   document exists when its dispatch objects were refused* — instead of as
//!   two identifiers in an order.
//!
//! The two are complementary and both are wanted: the structural guard fails
//! loudly when someone reorders the calls, this one fails when someone breaks
//! what the order is for.

use std::path::Path;

use ocx_lib::cli::ExitCode;
use ocx_lib::file_structure::IndexStore;
use ocx_lib::oci::Algorithm;
use serde_json::{Value, json};

use super::super::*;

/// The source subtree name — `output: <dir>/public` + `as: ocx.sh` ⇒
/// `<dir>/public/ocx.sh/{config.json,c/,p/}` (the wrapped layout, C-027).
const AS_NAME: &str = "ocx.sh";
/// The **logical** catalog key the tree is keyed on at both ends —
/// deliberately not the physical repository below.
const PACKAGE: &str = "kitware/cmake";
/// The rewritten `oci://` pointer the mirrored root must carry — over
/// `mirror/ocx.sh/kitware/cmake`, where the copied content actually lands.
const POINTER: &str = "oci://registry.test/mirror/ocx.sh/kitware/cmake";
/// The physical repository the *source* root points at, which the rewrite
/// replaces.
const UPSTREAM: &str = "oci://ghcr.io/ocx-contrib/kitware/cmake";

/// The publish identity every test here writes under: the one subtree, the one
/// catalog key, and whichever pointer the mode under test calls for.
fn publish_target(pointer: Option<&str>) -> PublishTarget<'_> {
    PublishTarget {
        as_name: AS_NAME,
        name: PACKAGE,
        pointer,
    }
}

/// A store over `<dir>/public`, locks redirected to `<dir>/locks` — outside the
/// served tree, which is what C-027 exists to guarantee.
async fn store_at(directory: &Path) -> (IndexStore, std::path::PathBuf) {
    let output = directory.join("public");
    tokio::fs::create_dir_all(&output)
        .await
        .expect("create the output tree");
    (
        index_write::build_index_store(&output, &directory.join("locks")),
        output,
    )
}

/// A minimal, valid OCI image index and the digest of its own bytes, so
/// `write_dispatch_object`'s verify passes.
fn image_index(seed: &str) -> (Digest, Vec<u8>) {
    let bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": Algorithm::Sha256.hash(seed.as_bytes()).to_string(),
            "size": 7023,
        }],
    }))
    .expect("serialize image index");
    (Algorithm::Sha256.hash(&bytes), bytes)
}

/// A bare OCI **image** manifest and its digest — the shape `o/` must never
/// hold (C-033), and therefore the cheapest way to make the *first* write of
/// the publish order fail.
fn image_manifest() -> (Digest, Vec<u8>) {
    let bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": Algorithm::Sha256.hash(b"config").to_string(),
            "size": 452,
        },
        "layers": [],
    }))
    .expect("serialize image manifest");
    (Algorithm::Sha256.hash(&bytes), bytes)
}

/// A root document in the shape an upstream index serves.
///
/// Every tag carries an `observed` timestamp the typed `RootTag` does not
/// model, so a merge that round-tripped a surviving tag through `IndexRoot`
/// would visibly drop it.
fn root_document(tags: &[(&str, &Digest)]) -> Vec<u8> {
    let mut document = serde_json::Map::new();
    document.insert("repository".to_string(), json!(UPSTREAM));
    document.insert("name".to_string(), json!("cmake"));
    document.insert("upstream".to_string(), json!("https://cmake.org"));
    let mut tag_map = serde_json::Map::new();
    for (tag, content) in tags {
        tag_map.insert(
            (*tag).to_string(),
            json!({ "content": content.to_string(), "observed": "2026-08-14T00:00:00Z" }),
        );
    }
    document.insert("tags".to_string(), Value::Object(tag_map));
    serialize_root(&Value::Object(document))
}

/// The `tags` map of a root document as `tag → content digest`, which is what
/// every merge assertion is actually about.
fn tag_digests(raw: &[u8]) -> Vec<(String, String)> {
    let document: Value = serde_json::from_slice(raw).expect("parse root document");
    document["tags"]
        .as_object()
        .expect("tags is an object")
        .iter()
        .map(|(tag, pointer)| {
            (
                tag.clone(),
                pointer["content"].as_str().expect("content is a string").to_string(),
            )
        })
        .collect()
}

/// The confirmed-tag set, spelled as the copy pass hands it over.
fn confirmed(tags: &[&str]) -> BTreeSet<String> {
    tags.iter().map(|tag| (*tag).to_string()).collect()
}

/// One dispatch object per content digest, keyed the way the copy pass collects
/// them.
fn objects(entries: &[(Digest, Vec<u8>)]) -> BTreeMap<Digest, Vec<u8>> {
    entries.iter().cloned().collect()
}

/// The root document currently published for [`PACKAGE`].
fn published_root(store: &IndexStore) -> Vec<u8> {
    std::fs::read(store.root_document_path(AS_NAME, PACKAGE)).expect("read the published root")
}

/// Every path under `root`, relative and sorted — the **recursive listing**
/// C-036 is asserted on, never a name allow-list.
fn list_recursive(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(directory) = queue.pop() {
        for entry in std::fs::read_dir(&directory).expect("read directory") {
            let path = entry.expect("directory entry").path();
            found.push(
                path.strip_prefix(root)
                    .expect("path under root")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            if path.is_dir() {
                queue.push(path);
            }
        }
    }
    found.sort();
    found
}

/// `rewrite_pointers: false` republishes the source's own `repository`, and
/// changes nothing else.
///
/// The strong form of the assertion is **byte equality against the source
/// document**, not a field comparison: preserving is meant to be the *absence*
/// of the rewrite, so a run whose tag set is fully confirmed must reproduce the
/// upstream bytes exactly. A field check would pass an implementation that
/// writes the upstream pointer back through a typed round trip — which is the
/// one thing `rewrite_root`'s own doc forbids, because it silently drops the
/// forward-compat fields this crate does not model.
///
/// Falsified by rewriting unconditionally: `repository` then reads [`POINTER`].
#[tokio::test]
async fn preserving_republishes_the_upstream_pointer_byte_for_byte() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, _output) = store_at(directory.path()).await;

    let (release, bytes) = image_index("3.28.1");
    let source = root_document(&[("3.28.1", &release)]);
    write_package(
        &store,
        publish_target(None),
        &source,
        None,
        &confirmed(&["3.28.1"]),
        &objects(&[(release.clone(), bytes)]),
    )
    .await
    .expect("the publish");

    let published = published_root(&store);
    let document: Value = serde_json::from_slice(&published).expect("parse root document");
    assert_eq!(
        document["repository"],
        json!(UPSTREAM),
        "the mirrored root must keep the pointer the source published"
    );
    assert_eq!(
        published, source,
        "preserving is the absence of the rewrite, so the bytes must round-trip unchanged"
    );
}

/// The same publish under `rewrite_pointers: true` — the pair is what makes the
/// switch's effect the *only* difference between the two outcomes.
#[tokio::test]
async fn rewriting_replaces_the_upstream_pointer_with_the_destination() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, _output) = store_at(directory.path()).await;

    let (release, bytes) = image_index("3.28.1");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &release)]),
        None,
        &confirmed(&["3.28.1"]),
        &objects(&[(release.clone(), bytes)]),
    )
    .await
    .expect("the publish");

    let document: Value = serde_json::from_slice(&published_root(&store)).expect("parse root document");
    assert_eq!(document["repository"], json!(POINTER));
}

/// **S-026, behaviourally — the regression every `index_write` test passes
/// without.**
///
/// Run 1 mirrors `{3.28.1, 3.29.0}` cleanly. Run 2 sees both tags re-pointed
/// upstream but only `3.28.1`'s content copies, so only `3.28.1` is confirmed.
/// `3.29.0` must survive **at run 1's digest**: the destination serves that
/// content and no other, so advertising the new digest would publish a pointer
/// to bytes nothing copied — and dropping the tag would delete a release an
/// earlier run mirrored.
///
/// Falsified by handing the *source's* tag set to `merge_root_tags` in place of
/// `confirmed`: `3.29.0` then reads the uncopied digest.
#[tokio::test]
async fn a_tag_whose_copy_failed_keeps_the_digest_the_last_good_run_published() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, _output) = store_at(directory.path()).await;

    let (first_release, first_bytes) = image_index("3.28.1");
    let (second_release, second_bytes) = image_index("3.29.0");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &first_release), ("3.29.0", &second_release)]),
        None,
        &confirmed(&["3.28.1", "3.29.0"]),
        &objects(&[
            (first_release.clone(), first_bytes),
            (second_release.clone(), second_bytes),
        ]),
    )
    .await
    .expect("the first publish");

    let after_first = published_root(&store);
    assert_eq!(
        tag_digests(&after_first),
        vec![
            ("3.28.1".to_string(), first_release.to_string()),
            ("3.29.0".to_string(), second_release.to_string()),
        ]
    );

    // Upstream re-points both tags; only the first one's content copies.
    let (first_moved, first_moved_bytes) = image_index("3.28.1-rebuilt");
    let (second_moved, _never_copied) = image_index("3.29.0-rebuilt");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &first_moved), ("3.29.0", &second_moved)]),
        Some(&after_first),
        &confirmed(&["3.28.1"]),
        &objects(&[(first_moved.clone(), first_moved_bytes)]),
    )
    .await
    .expect("the partial publish");

    assert_eq!(
        tag_digests(&published_root(&store)),
        vec![
            ("3.28.1".to_string(), first_moved.to_string()),
            ("3.29.0".to_string(), second_release.to_string()),
        ],
        "the confirmed tag updates; the failed one keeps the digest run 1 published"
    );
}

/// C-030's second invariant, stated as what it is for: `root exists ⇒ its
/// dispatch objects exist`.
///
/// A bare image manifest under a `content` pointer is refused by
/// `write_dispatch_objects`, the **first** write of the order, so the package
/// must end with no root at all. Written the other way round, the root lands
/// naming a `content` digest with no object under `o/` — which satisfies all
/// three of C-032's skip conditions forever, and which `--repair-catalog`
/// cannot repair because it only ever writes `c/index.json`.
#[tokio::test]
async fn a_refused_dispatch_object_leaves_no_root_document_behind() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, _output) = store_at(directory.path()).await;

    let (bare, bare_bytes) = image_manifest();
    let error = write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &bare)]),
        None,
        &confirmed(&["3.28.1"]),
        &objects(&[(bare, bare_bytes)]),
    )
    .await
    .expect_err("`o/` is indices-only by format invariant");

    // Upstream's bytes, so it fails the package rather than the run.
    assert_eq!(error.kind_exit_code(), ExitCode::Failure, "{error:?}");
    assert!(
        !store.root_document_path(AS_NAME, PACKAGE).exists(),
        "the root must not be written when its dispatch objects were refused"
    );
    assert!(
        !store.source_catalog_path(AS_NAME).exists(),
        "and no catalog entry may name it either"
    );

    // The two negative assertions above are only worth anything if that path
    // can hold a file at all: a typo in `AS_NAME` or `PACKAGE` would satisfy
    // both forever. Publishing the same package with a valid object puts one
    // there, so the red state is reachable inside this test.
    let (index, index_bytes) = image_index("3.28.1");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &index)]),
        None,
        &confirmed(&["3.28.1"]),
        &objects(&[(index, index_bytes)]),
    )
    .await
    .expect("a valid dispatch object publishes");
    assert!(
        store.root_document_path(AS_NAME, PACKAGE).exists(),
        "the path the assertions above deny must be one a root can actually occupy"
    );
}

/// The whole publish order, observed by its outputs: dispatch object, root,
/// catalog entry, `config.json`.
///
/// The catalog entry is `sha256(root bytes)`, derived by the transaction that
/// wrote the root — which is what proves the write went through
/// `CatalogTransaction` rather than the bare document writer that leaves the
/// catalog behind.
#[tokio::test]
async fn a_published_package_lands_its_object_root_catalog_entry_and_config() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, output) = store_at(directory.path()).await;

    let (release, release_bytes) = image_index("3.28.1");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &release)]),
        None,
        &confirmed(&["3.28.1"]),
        &objects(&[(release.clone(), release_bytes)]),
    )
    .await
    .expect("publish");

    assert!(
        store.dispatch_object_path(AS_NAME, PACKAGE, &release).exists(),
        "the dispatch object is what a consumer resolves the tag through"
    );

    let root = published_root(&store);
    let document: Value = serde_json::from_slice(&root).expect("parse the root");
    assert_eq!(
        document["repository"], POINTER,
        "the pointer is rewritten to the mirror"
    );
    assert_eq!(
        document["upstream"], "https://cmake.org",
        "human-governed fields ride through verbatim"
    );

    let catalog: Value =
        serde_json::from_slice(&std::fs::read(store.source_catalog_path(AS_NAME)).expect("read the catalog"))
            .expect("parse the catalog");
    assert_eq!(catalog["packages"][PACKAGE], IndexStore::root_catalog_entry(&root));

    let config: Value =
        serde_json::from_slice(&std::fs::read(store.source_config_path(AS_NAME)).expect("read config.json"))
            .expect("parse config.json");
    assert_eq!(config["format_version"], 1);
    assert!(
        config.get("name_segments").is_none(),
        "ocx never guesses a name grammar, and upstream's is never copied into ours"
    );

    // C-036 on the recursive listing, so a stray `locks/` shows up here rather
    // than in an operator's committed tree.
    let listing = list_recursive(&output);
    assert!(
        listing.iter().all(|path| path.starts_with("ocx.sh")),
        "unexpected entries under output: {listing:?}"
    );
    assert!(
        !listing.iter().any(|path| path.contains("locks")),
        "locks must live outside the served tree: {listing:?}"
    );
}

/// A destination root that will not parse fails its package and **changes
/// nothing on disk**.
///
/// The alternative — reading the failure as "no destination root" — degrades
/// the union to this run's confirmed set and deletes every tag a previous run
/// mirrored, which is the precise deletion C-047 exists to prevent. `Ok(None)`
/// and `Err(_)` are not the same thing.
#[tokio::test]
async fn an_unreadable_destination_root_fails_the_package_without_truncating_it() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, _output) = store_at(directory.path()).await;

    let (release, release_bytes) = image_index("3.28.1");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &release)]),
        None,
        &confirmed(&["3.28.1"]),
        &objects(&[(release.clone(), release_bytes)]),
    )
    .await
    .expect("the first publish");
    let before = published_root(&store);

    let (moved, moved_bytes) = image_index("3.28.1-rebuilt");
    let error = write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &moved)]),
        Some(b"{ this is not a root document"),
        &confirmed(&["3.28.1"]),
        &objects(&[(moved, moved_bytes)]),
    )
    .await
    .expect_err("a corrupt destination root is refused, never read as absent");

    assert_eq!(error.kind_exit_code(), ExitCode::Failure, "{error:?}");
    assert_eq!(published_root(&store), before, "the published root is untouched");
}

/// An empty `confirmed` set republishes exactly what the destination already
/// held — no tag invented, none dropped.
///
/// That is the shape of a run where every tag's content copy failed against a
/// package that *was* published before, and the one case where writing the root
/// is still right: the package-level fields legitimately update.
#[tokio::test]
async fn nothing_confirmed_republishes_the_destinations_own_tags() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let (store, _output) = store_at(directory.path()).await;

    let (release, release_bytes) = image_index("3.28.1");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &release)]),
        None,
        &confirmed(&["3.28.1"]),
        &objects(&[(release.clone(), release_bytes)]),
    )
    .await
    .expect("the first publish");
    let after_first = published_root(&store);

    let (moved, _never_copied) = image_index("3.28.1-rebuilt");
    write_package(
        &store,
        publish_target(Some(POINTER)),
        &root_document(&[("3.28.1", &moved)]),
        Some(&after_first),
        &confirmed(&[]),
        &objects(&[]),
    )
    .await
    .expect("a publish that confirmed nothing");

    assert_eq!(
        tag_digests(&published_root(&store)),
        vec![("3.28.1".to_string(), release.to_string())],
        "the tag keeps the digest the destination actually holds"
    );
}
