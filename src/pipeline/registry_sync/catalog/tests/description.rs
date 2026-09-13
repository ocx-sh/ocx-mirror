// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-048 — the README/logo objects a root's `desc` names, fetched from the
//! source tree beside the root (ocx-mirror#69): which paths are tried, which
//! bytes come back, and which failures are the source's own bytes refusing to
//! validate (one package) rather than a read that did not answer (the run).

use ocx_lib::cli::ExitCode;
use ocx_lib::oci::Algorithm;

use super::super::*;
use super::support::*;

const PACKAGE: &str = "kitware/cmake";
const README: &str = "# cmake\n\nMirrored.\n";
const LOGO: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\"/>";

async fn client_for(index: &TestIndex) -> reqwest::Client {
    build_source_index_client(&index.base_url(), &loopback_trusted())
        .await
        .expect("a trusted loopback host builds a client")
}

fn sha256(bytes: &str) -> Digest {
    Algorithm::Sha256.hash(bytes.as_bytes())
}

/// `p/kitware/cmake/o/sha256/<hex>.<ext>` — the path the tree serves an object
/// under, and the one the mirrored tree must serve it under too.
fn object_path(digest: &Digest, extension: &str) -> String {
    format!("/p/{PACKAGE}/o/sha256/{}.{extension}", digest.hex())
}

/// A root whose `desc` names `readme` and `logo` — either may be `null`, and
/// the `desc` object carries the fields the mirror does not model, as a real
/// published root does.
fn described_root(readme: Option<&Digest>, logo: Option<&Digest>) -> Vec<u8> {
    let pointer = |digest: Option<&Digest>| digest.map_or(serde_json::Value::Null, |d| d.to_string().into());
    serde_json::to_vec(&serde_json::json!({
        "repository": "oci://ghcr.io/kitware/cmake",
        "tags": {"3.28.1": {"content": FIXTURE_DIGEST}},
        "desc": {
            "digest": "sha256:0000",
            "title": "cmake",
            "readme": pointer(readme),
            "logo": pointer(logo),
        },
    }))
    .expect("a root serializes")
}

#[tokio::test]
async fn the_readme_and_logo_come_back_verified_under_the_extension_the_tree_serves() {
    let (readme, logo) = (sha256(README), sha256(LOGO));
    let index = TestIndex::start(vec![
        Route::json(&object_path(&readme, "md"), README),
        Route::json(&object_path(&logo, "svg"), LOGO),
    ])
    .await;
    let client = client_for(&index).await;

    let objects = fetch_description_objects(
        &client,
        &index.base_url(),
        PACKAGE,
        &described_root(Some(&readme), Some(&logo)),
    )
    .await
    .expect("both objects are served");

    assert_eq!(
        objects,
        vec![
            DescriptionObject {
                digest: readme.clone(),
                extension: "md",
                bytes: README.as_bytes().to_vec(),
            },
            DescriptionObject {
                digest: logo.clone(),
                extension: "svg",
                bytes: LOGO.as_bytes().to_vec(),
            },
        ]
    );
    assert_eq!(
        index.paths(),
        vec![object_path(&readme, "md"), object_path(&logo, "svg")],
        "one request per object, under the first extension that answers"
    );
}

#[tokio::test]
async fn a_logo_the_tree_serves_as_png_is_found_after_svg_misses() {
    // A digest carries no extension and an HTTP tree cannot be listed, so the
    // logo is tried under indexbot's own set in order: `.svg`, then `.png`.
    let logo = sha256(LOGO);
    let index = TestIndex::start(vec![Route::json(&object_path(&logo, "png"), LOGO)]).await;
    let client = client_for(&index).await;

    let objects = fetch_description_objects(&client, &index.base_url(), PACKAGE, &described_root(None, Some(&logo)))
        .await
        .expect("the png answers");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].extension, "png");
    assert_eq!(
        index.paths(),
        vec![object_path(&logo, "svg"), object_path(&logo, "png")],
        "svg is probed first, and the 404 is not an error"
    );
}

#[tokio::test]
async fn a_root_without_a_desc_fetches_nothing() {
    let index = TestIndex::start(Vec::new()).await;
    let client = client_for(&index).await;
    let bare = br#"{"repository":"oci://ghcr.io/kitware/cmake","tags":{}}"#;

    let objects = fetch_description_objects(&client, &index.base_url(), PACKAGE, bare)
        .await
        .expect("nothing to fetch is not a failure");

    assert!(objects.is_empty());
    assert!(
        index.paths().is_empty(),
        "a package without a description costs no request"
    );
}

#[tokio::test]
async fn a_desc_with_null_pointers_fetches_nothing() {
    let index = TestIndex::start(Vec::new()).await;
    let client = client_for(&index).await;

    let objects = fetch_description_objects(&client, &index.base_url(), PACKAGE, &described_root(None, None))
        .await
        .expect("null pointers name nothing");

    assert!(objects.is_empty());
    assert!(index.paths().is_empty());
}

#[tokio::test]
async fn an_object_absent_under_every_extension_fails_the_package_not_the_run() {
    // The dangling reference upstream (ocx-mirror#68's own shape): the root
    // names a digest the tree does not hold. That is the source's bytes
    // refusing to validate, so it is one package's failure — a hostile or
    // broken upstream must not deny the whole mirror through one root.
    let readme = sha256(README);
    let index = TestIndex::start(Vec::new()).await;
    let client = client_for(&index).await;

    let error = fetch_description_objects(
        &client,
        &index.base_url(),
        PACKAGE,
        &described_root(Some(&readme), None),
    )
    .await
    .expect_err("a root naming an object the tree lacks is refused");

    let MirrorError::ExecutionFailed(messages) = &error else {
        panic!("a dangling reference is the aggregating class, got {error:?}");
    };
    assert_eq!(error.kind_exit_code(), ExitCode::Failure);
    assert!(
        messages[0].contains("desc.readme") && messages[0].contains(&readme.to_string()),
        "the refusal names the field and the digest: {messages:?}"
    );
    assert_eq!(
        index.paths(),
        vec![object_path(&readme, "md")],
        "the readme has one extension to try"
    );
}

#[tokio::test]
async fn a_body_that_does_not_hash_to_its_digest_fails_the_package_not_the_run() {
    let readme = sha256(README);
    let index = TestIndex::start(vec![Route::json(&object_path(&readme, "md"), "# not the readme\n")]).await;
    let client = client_for(&index).await;

    let error = fetch_description_objects(
        &client,
        &index.base_url(),
        PACKAGE,
        &described_root(Some(&readme), None),
    )
    .await
    .expect_err("integrity is verified on receipt, never delegated to the peer");

    assert!(matches!(error, MirrorError::ExecutionFailed(_)), "got {error:?}");
    assert!(error.to_string().contains("hashes to"), "{error}");
}

#[tokio::test]
async fn a_pointer_that_is_not_a_digest_fails_the_package_not_the_run() {
    let index = TestIndex::start(Vec::new()).await;
    let client = client_for(&index).await;
    let root = br#"{"repository":"oci://ghcr.io/kitware/cmake","tags":{},"desc":{"readme":"../../etc/passwd"}}"#;

    let error = fetch_description_objects(&client, &index.base_url(), PACKAGE, root)
        .await
        .expect_err("a pointer that is not a digest never becomes a path");

    assert!(matches!(error, MirrorError::ExecutionFailed(_)), "got {error:?}");
    assert!(index.paths().is_empty(), "refused before any request");
}

#[tokio::test]
async fn a_desc_of_the_wrong_shape_fails_the_package_not_the_run() {
    let index = TestIndex::start(Vec::new()).await;
    let client = client_for(&index).await;
    let root = br#"{"repository":"oci://ghcr.io/kitware/cmake","tags":{},"desc":"sha256:0000"}"#;

    let error = fetch_description_objects(&client, &index.base_url(), PACKAGE, root)
        .await
        .expect_err("a desc that is not an object is foreign data that will not validate");

    assert!(matches!(error, MirrorError::ExecutionFailed(_)), "got {error:?}");
}

#[tokio::test]
async fn a_read_that_did_not_answer_aborts_the_run() {
    // Only a 404 reads as absence; anything else is a read with no
    // authoritative answer, and that class aborts the run — the same class the
    // root fetch itself is.
    let readme = sha256(README);
    let index = TestIndex::start(vec![Route::status(
        &object_path(&readme, "md"),
        "500 Internal Server Error",
    )])
    .await;
    let client = client_for(&index).await;

    let error = fetch_description_objects(
        &client,
        &index.base_url(),
        PACKAGE,
        &described_root(Some(&readme), None),
    )
    .await
    .expect_err("a 500 is not an absence");

    assert!(matches!(error, MirrorError::SourceError(_)), "got {error:?}");
    assert_eq!(error.kind_exit_code(), ExitCode::Unavailable);
}
