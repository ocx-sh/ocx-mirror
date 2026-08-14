// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `index_write` test module.

use std::path::PathBuf;

use ocx_lib::oci::Algorithm;
use serde_json::{Value, json};

use super::super::*;

/// The source subtree name — `output: <dir>/public` + `as: ocx.sh` ⇒
/// `<dir>/public/ocx.sh/{config.json,c/,p/}` (the wrapped layout, C-027).
pub const AS_NAME: &str = "ocx.sh";
/// The **logical** catalog key a package is served under, at both ends.
pub const PACKAGE: &str = "tools/cmake";
/// The rewritten pointer: the destination registry plus the composed physical
/// repository, which is deliberately not the logical name.
pub const POINTER: &str = "oci://registry.test/mirror/ocx.sh/tools/cmake";

/// A store over `<dir>/public`, locks redirected to `<dir>/locks` — outside the
/// served tree, which is what C-027 exists to guarantee.
pub fn store(dir: &std::path::Path) -> (IndexStore, PathBuf) {
    let output = dir.join("public");
    let store = build_index_store(&output, &dir.join("locks"));
    (store, output)
}

/// `sha256` of a short unique string — a stand-in for a manifest digest in
/// tests that never write the manifest itself.
pub fn digest(seed: &str) -> Digest {
    Algorithm::Sha256.hash(seed.as_bytes())
}

/// A minimal, valid OCI image index and the digest of its own bytes, so
/// `write_dispatch_object`'s verify passes.
pub fn image_index(child: &str) -> (Digest, Vec<u8>) {
    let bytes = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest(child).to_string(),
            "size": 7023,
        }],
    });
    let bytes = serde_json::to_vec(&bytes).expect("serialize image index");
    (Algorithm::Sha256.hash(&bytes), bytes)
}

/// A bare OCI **image** manifest — the shape `o/` must never hold (C-033).
pub fn image_manifest() -> (Digest, Vec<u8>) {
    let bytes = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": digest("config").to_string(),
            "size": 452,
        },
        "layers": [],
    });
    let bytes = serde_json::to_vec(&bytes).expect("serialize image manifest");
    (Algorithm::Sha256.hash(&bytes), bytes)
}

/// A root document in the shape an upstream index serves: `repository` first,
/// two human-governed fields, then `tags`. The key order is deliberately **not
/// alphabetical**, so an order assertion can tell insertion order from a
/// `BTreeMap`'s — the `preserve_order` regression has no compile error.
///
/// Every tag carries an `observed` timestamp, which the typed `RootTag` does
/// not model: a merge that round-tripped a surviving tag through `IndexRoot`
/// would drop it.
pub fn root_document(repository: &str, tags: &[(&str, &Digest)]) -> Vec<u8> {
    let mut document = serde_json::Map::new();
    document.insert("repository".to_string(), json!(repository));
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

/// The top-level key order of a root document, in the order it is emitted.
pub fn key_order(raw: &[u8]) -> Vec<String> {
    let document: Value = serde_json::from_slice(raw).expect("parse root document");
    document
        .as_object()
        .expect("root document is an object")
        .keys()
        .cloned()
        .collect()
}

/// The `tags` map of a root document as `tag → content digest`, dropping the
/// sibling fields so a test can assert the pointer set alone.
pub fn tag_digests(raw: &[u8]) -> Vec<(String, String)> {
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

/// Parse a root document's bytes into the typed shape the skip predicate takes.
pub fn parse_root(raw: &[u8]) -> IndexRoot {
    serde_json::from_slice(raw).expect("parse root document")
}

/// A [`RootReadResult`] over `raw`, as `read_root_uncatalogued` returns one.
pub fn read_result(raw: &[u8]) -> RootReadResult {
    RootReadResult {
        bytes: raw.to_vec(),
        root: parse_root(raw),
        catalog_status: ocx_lib::file_structure::CatalogEntryStatus::NoCatalog,
    }
}

/// Every path under `root`, relative and sorted — the **recursive listing**
/// C-036 is asserted on, never a name allow-list.
pub fn list_recursive(root: &std::path::Path) -> Vec<String> {
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

/// The message of an **aggregating-class** refusal (C-040, exit 1), panicking
/// if the error came back in the whole-run-abort class instead. Foreign data
/// that will not validate must never abort the run.
pub fn refusal_message(error: &MirrorError) -> String {
    match error {
        MirrorError::ExecutionFailed(messages) => messages.concat(),
        other => panic!("expected C-040's aggregating class, got {other:?}"),
    }
}

/// The confirmed-tag set for a run, spelled as the caller passes it.
pub fn confirmed(tags: &[&str]) -> std::collections::BTreeSet<String> {
    tags.iter().map(|tag| (*tag).to_string()).collect()
}
