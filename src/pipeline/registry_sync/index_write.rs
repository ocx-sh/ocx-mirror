// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Writing the servable index tree: store construction, the root rewrite and
//! its tag merge, the catalog transaction, dispatch objects, `config.json`, the
//! skip predicate, and `--repair-catalog`.
//!
//! **Writes here are additive.** `CatalogTransaction::write_root` is
//! merge-blind — it persists exactly the bytes handed to it — and the
//! never-delete guarantee lives in `merge_root`, which is unreachable from this
//! crate. So the mirror owns its own merge (C-047), or a `continue`-on-error run
//! would write a root holding only the tags that succeeded and silently delete
//! tags a previous run had mirrored.
//!
//! # Call order for one package
//!
//! The order is the visibility guarantee (C-030), not a style preference:
//!
//! ```text
//! write_dispatch_objects(…)                       // root exists ⇒ its objects exist
//! let merged = merge_root_tags(&source, dest, &confirmed)?;   // C-047
//! let rewritten = rewrite_root(&serialize_root(&merged), &pointer)?;  // C-028
//! let mut transaction = store.begin_catalog_transaction(as_name).await?;
//! write_root(&mut transaction, name, &rewritten).await?;      // C-029
//! transaction.commit().await?;
//! write_config_json(&store, as_name).await?;                  // C-031, AFTER commit
//! ```
//!
//! [`merge_root_tags`] and [`rewrite_root`] are separate because only one of
//! them is a byte-level transform: the merge reasons about two documents, the
//! rewrite mutates one key. `serialize_root` bridges them, and the bridge is
//! free of drift because both ends are the same order-preserving `Value`.
//!
//! Contracts C-027…C-036 and C-047 of `plan_registry_mirror_sync.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ocx_lib::file_structure::{CatalogTransaction, IndexStore, RootReadResult, SOURCE_LOCK_TIMEOUT};
use ocx_lib::log;
use ocx_lib::oci::index::{
    CatalogIndex, IndexFormatConfig, IndexRoot, RegenerateOutcome, SUPPORTED_FORMAT_VERSION, parse_physical_repository,
    regenerate_catalog, serialize_config, serialize_root,
};
use ocx_lib::oci::manifest::validate_image_index;
use ocx_lib::oci::{Digest, ImageIndex};

use crate::error::MirrorError;

/// Build the store for one output tree, with locks redirected out of it
/// (C-027).
///
/// `with_locks_root` is **mandatory**, and forgetting it is silent:
/// `IndexStore::new` otherwise defaults `locks_root` to `root.join("locks")`,
/// creating a lock directory inside the tree the operator serves and commits.
/// `locks_dir` comes from [`super::cache::locks_dir`] and is stable per output
/// tree, never per run.
pub fn build_index_store(output: &Path, locks_dir: &Path) -> IndexStore {
    IndexStore::new(output).with_locks_root(locks_dir)
}

/// A refusal of **upstream bytes** — C-040's aggregating class (exit 1).
///
/// The two classes this module returns are not interchangeable, and the split
/// is by *whose fault it is*, not by which function failed:
///
/// - **Foreign data that will not validate** (a root that is not an object, a
///   `content` pointer whose manifest is not an image index) is per package.
///   Aborting the run would let any upstream — hostile ones are in scope per
///   the threat model — deny a 121-package mirror by publishing one malformed
///   package. That is exactly what `on_error: continue` exists for.
/// - **A local filesystem write failure** (disk full, `EACCES`, a broken output
///   tree) stays [`MirrorError::IndexWriteError`] (exit 74) and aborts: there is
///   no meaningful way to continue writing a tree that cannot be written.
///
/// Digest verification is deliberately *not* in the first class here: the copy
/// engine (C-022) verifies every digest before these bytes arrive, so the
/// store's own re-verification failing means local corruption, not upstream.
fn refused(message: String) -> MirrorError {
    MirrorError::ExecutionFailed(vec![message])
}

/// Rewrite a source root's `repository` pointer (C-028).
///
/// Parses the raw bytes to `serde_json::Value`, mutates `repository` to
/// `pointer`, and re-serializes through `wire_writer::serialize_root`. That one
/// key is the only thing touched; everything else rides through verbatim,
/// including its position — `serde_json`'s `preserve_order` map re-inserts an
/// existing key in place.
///
/// A **pure byte-level transform**: it does not apply [`merge_root_tags`]. The
/// caller composes the two, so the merge stays testable on its own and this
/// function stays the one place `repository` is written.
///
/// **A typed round-trip is forbidden.** `IndexRoot` is `Deserialize`-only and
/// carries no `deny_unknown_fields` for fleet forward-compat, so a typed round
/// trip would silently drop newer fields the mirror must pass through. This is
/// the mutation shape upstream documents as supported — `serialize_root`'s own
/// doc describes announce mutating only `tags` and letting every human-governed
/// field ride through the `Value`; the mirror is that pattern with
/// `repository` in place of `tags`.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] (exit 1, C-040's **aggregating** class)
/// when the source bytes are not a JSON object — these are upstream bytes, so
/// one malformed package fails its own package and never the run.
pub fn rewrite_root(raw: &[u8], pointer: &str) -> Result<Vec<u8>, MirrorError> {
    let mut document: serde_json::Value =
        serde_json::from_slice(raw).map_err(|error| refused(format!("root document is not valid JSON: {error}")))?;
    let fields = document
        .as_object_mut()
        .ok_or_else(|| refused("root document is not a JSON object".to_string()))?;
    fields.insert("repository".to_string(), serde_json::Value::String(pointer.to_string()));
    Ok(serialize_root(&document))
}

/// The mirror's own `tags{}` merge — **the guard that makes writes additive**
/// (C-047).
///
/// - package-level fields (`name`, `desc`, `owners`, `created`, `upstream`,
///   `status`, `deprecated_message`, `superseded_by`, **and anything unknown**)
///   come from the **fresh source fetch**; those legitimately update.
/// - `tags` is a **union**: `destination existing tags ∪ this run's confirmed
///   tags`, this run winning on conflict. Note *confirmed*, not *source* — a
///   source tag whose content copy failed must not appear, so the source's
///   `tags` is filtered down to `confirmed` **before** the union.
///
/// `repository` is left as the source served it; [`rewrite_root`] owns that
/// key.
///
/// Operates over `serde_json::Value` end to end so forward-compat fields ride
/// through on **both** sides. `destination_root` is therefore the destination
/// root's **verbatim bytes**, not a parsed `IndexRoot`: `RootTag` is
/// `Deserialize`-only and models `content` + `yanked` alone, so re-emitting a
/// surviving tag from the typed shape would hand-roll wire JSON and drop the
/// `observed` timestamp every hosted root carries. `RootReadResult.bytes` is
/// exactly this parameter.
///
/// `None` is a **first run** and degrades to confirmed-tags-only. **`None` and
/// a read error are not the same thing and the caller must not collapse them**:
/// mapping a failed `IndexStore::read_root_uncatalogued` to `None` truncates
/// `tags{}` on a corrupt root — precisely the deletion this contract exists to
/// prevent. A read error fails the package instead.
///
/// The trap this creates, named so it is not discovered in the field:
/// union-with-this-run-wins self-heals any tag still upstream, but cannot
/// remove a tag upstream **deletes** — that tag lives at the destination
/// forever. Accepted; the index is append-only by ruling. Do not add a delete
/// verb.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] (exit 1, C-040's **aggregating** class) when
/// either document is not a JSON object — including the corrupt-destination
/// case, which fails its package precisely so it cannot degrade to
/// confirmed-only.
pub fn merge_root_tags(
    source_value: &serde_json::Value,
    destination_root: Option<&[u8]>,
    confirmed: &BTreeSet<String>,
) -> Result<serde_json::Value, MirrorError> {
    let mut merged = source_value.clone();
    let fields = merged
        .as_object_mut()
        .ok_or_else(|| refused("source root document is not a JSON object".to_string()))?;

    // Destination first, in its on-disk order, so a tag present at both ends
    // keeps the position the mirror already published and an unchanged package
    // re-serializes to the same bytes.
    let mut tags = serde_json::Map::new();
    if let Some(bytes) = destination_root {
        let existing: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| refused(format!("destination root document is not valid JSON: {error}")))?;
        for (tag, pointer) in root_tags(&existing, "destination")?.into_iter().flatten() {
            tags.insert(tag.clone(), pointer.clone());
        }
    }
    for (tag, pointer) in root_tags(source_value, "source")?.into_iter().flatten() {
        if confirmed.contains(tag) {
            tags.insert(tag.clone(), pointer.clone());
        }
    }
    fields.insert("tags".to_string(), serde_json::Value::Object(tags));
    Ok(merged)
}

/// Warn when the destination root's `repository` disagrees with the pointer
/// this run computed — C-047's free drift diagnostic.
///
/// The destination root is already in hand for [`merge_root_tags`], so the
/// comparison costs nothing. It is also the only way to notice that
/// `destination:` or `target` was edited after first publish, which otherwise
/// silently re-homes every package on the next run. **Both** values are named,
/// because either one could be the mistake.
///
/// A diagnostic, not a gate: the copy still proceeds.
///
/// Every string in the line is `{:?}`-formatted. `name` is a catalog key and
/// `published` comes out of a document the source authored, so both are foreign
/// data on their way into a log an operator reads — a newline in either forges
/// log lines and `\u{202e}` reverses what the terminal shows (CWE-117).
pub fn warn_on_pointer_drift(destination_root: Option<&[u8]>, name: &str, pointer: &str) {
    if let Some(published) = pointer_drift(destination_root, pointer) {
        log::warn!(
            "{name:?} is published at {published:?} but this run computes {pointer:?} — \
             `destination:` or `target` changed after first publish"
        );
    }
}

/// The destination's published `repository`, when it differs from `pointer`.
///
/// Unparseable bytes and a missing `repository` answer `None` rather than
/// warning: this is a diagnostic over bytes [`merge_root_tags`] is about to
/// reject loudly, so a second message would only add noise ahead of the real
/// error.
fn pointer_drift(destination_root: Option<&[u8]>, pointer: &str) -> Option<String> {
    let document: serde_json::Value = serde_json::from_slice(destination_root?).ok()?;
    let published = document.get("repository")?.as_str()?;
    (published != pointer).then(|| published.to_string())
}

/// The `tags` object of a root document, for [`merge_root_tags`].
///
/// Absent or `null` reads as no tags — `IndexRoot::tags` is `#[serde(default)]`,
/// so a root without the key is legal and has nothing to preserve. A present
/// **non-object** is refused instead of read as empty: the silent reading would
/// drop every tag on that side, which is exactly the deletion C-047 exists to
/// prevent.
fn root_tags<'document>(
    document: &'document serde_json::Value,
    side: &str,
) -> Result<Option<&'document serde_json::Map<String, serde_json::Value>>, MirrorError> {
    match document.get("tags") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Object(tags)) => Ok(Some(tags)),
        Some(_) => Err(refused(format!("{side} root document has a non-object tags field"))),
    }
}

/// Write one package's root inside the held transaction (C-029).
///
/// Uses `CatalogTransaction::write_root` — **not** the bare
/// `write_root_document`: it writes the bytes atomically *and* upserts the
/// derived catalog entry under the one held lock. The `repository_check` hook
/// is `parse_physical_repository`, the same one ocx's own published-root writer
/// passes; a no-op `|_| Ok(())` hook is forbidden, because it discards the one
/// cheap guarantee that a rewrite producing an unparseable pointer fails at the
/// write rather than shipping.
///
/// **The transaction is per package, not per run** — upstream's own contract
/// says all network work must happen before it is opened, and a per-package
/// transaction is what makes each package atomically visible on its own.
///
/// `repository` is the **logical** catalog key (`<ns>/<pkg>`), the same name
/// the source index serves the package under. The destination's physical
/// repository appears only inside the rewritten `repository` pointer.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] (exit 74) for an I/O failure or a pointer
/// that fails `parse_physical_repository`.
pub async fn write_root(
    transaction: &mut CatalogTransaction<'_>,
    repository: &str,
    rewritten: &[u8],
) -> Result<(), MirrorError> {
    transaction
        .write_root(repository, rewritten, |root| {
            parse_physical_repository(&root.repository).map(|_| ())
        })
        .await
        .map_err(|error| {
            MirrorError::IndexWriteError(format!("failed to write the root document for {repository:?}: {error}"))
        })
}

/// Write the dispatch objects for one package — **image indexes only**
/// (C-033).
///
/// For each distinct `content` digest in `tags{}`, the verified bytes the copy
/// engine already pulled are written with `IndexStore::write_dispatch_object`.
/// One fetch serves both the registry push and the index write.
///
/// The shape check is not optional and is enforced **here**, not at the call
/// site: `o/` is indices-only by format invariant, `write_dispatch_object`
/// verifies the digest but **not** the shape, and the upstream registry is in
/// scope per the threat model — so a source serving a bare image manifest under
/// a `content` pointer would otherwise poison the tree with a document no
/// reader accepts. `ImageIndex` deserialization requires `manifests`, so a leaf
/// manifest cannot parse; `validate_image_index` then rejects an index that
/// parses but is semantically malformed (wrong `schemaVersion`, an empty
/// `mediaType`/`digest`, a negative `size`).
///
/// A refusal is an error, never a skip. Skipping would let the root land naming
/// a `content` digest with no object under `o/`, which satisfies all three of
/// [`should_skip`]'s conditions forever — the package would be permanently
/// broken and permanently skipped.
///
/// Written **before** [`write_root`], never after: `root exists ⇒ its dispatch
/// objects exist`. Written after, a root that lands while a dispatch write does
/// not is skipped forever, and `--repair-catalog` cannot help because
/// `regenerate_catalog` only writes `c/index.json`. The ordering is free —
/// `write_dispatch_object` is content-addressed and idempotent.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] (exit 1, C-040's **aggregating** class) for
/// bytes that are not a valid OCI image index — see [`refused`] for why a shape
/// refusal must not abort the run. The store's own failures, including its
/// digest re-verification, stay [`MirrorError::IndexWriteError`] (exit 74).
/// Takes the caller's map by reference rather than a slice of pairs: the
/// orchestrator accumulates these in a `BTreeMap`, and a `&[(K, V)]` parameter
/// forced it to rebuild the whole thing — cloning every manifest body — purely
/// to change the container's shape. The body iterates identically either way.
pub async fn write_dispatch_objects(
    store: &IndexStore,
    as_name: &str,
    repository: &str,
    objects: &BTreeMap<Digest, Vec<u8>>,
) -> Result<(), MirrorError> {
    for (digest, bytes) in objects {
        let index: ImageIndex = serde_json::from_slice(bytes).map_err(|error| {
            refused(format!(
                "refusing a dispatch object for {repository:?} at {digest}: not an OCI image index: {error}"
            ))
        })?;
        validate_image_index(&index).map_err(|error| {
            refused(format!(
                "refusing a dispatch object for {repository:?} at {digest}: {error}"
            ))
        })?;
        store
            .write_dispatch_object(as_name, repository, digest, bytes)
            .await
            .map_err(|error| {
                MirrorError::IndexWriteError(format!(
                    "failed to write the dispatch object for {repository:?} at {digest}: {error}"
                ))
            })?;
    }
    Ok(())
}

/// Write `config.json` for one source subtree — **write-if-absent, after
/// `commit`, always synthesized** (C-031).
///
/// `IndexStore::ensure_source_config` is `pub(crate)` and unreachable, so the
/// mirror writes the file itself, with upstream's semantics rather than an
/// unconditional write. Unconditional writing breaks two things: the atomic
/// rename churns mtime even for byte-identical content, which is what makes a
/// scheduled run safe against a committed tree; and it clobbers an
/// operator-authored `name_segments`, which ocx explicitly refuses to guess.
/// The corrupt-file case belongs to `--repair-catalog`, not to every sync.
///
/// **The content is `{"format_version": 1}`, synthesized here, never copied
/// from the source's bytes.** `config.json` is *our* tree's fleet-facing
/// name-grammar declaration, not a document being mirrored — a hostile or
/// merely wrong `name_segments` copied from upstream changes how every client
/// in the fleet resolves names against this mirror, and the upstream index is
/// in scope per the threat model. Which is why this function takes no source
/// bytes at all: there is no parameter through which they could arrive.
/// Fetching the source's `config.json` is still necessary — that is C-018's
/// `format_version` gate — but its bytes are read, not written.
///
/// **Ordering: after `transaction.commit()`, not before.** That is upstream's
/// own order, and the reason is mechanical — this write re-acquires the same
/// source lock rather than inheriting the transaction's, so the inverted order
/// self-deadlocks. The lock scope and discriminator are upstream's verbatim
/// (`"index-catalog"` / `"c/index.json"`); a `"config.json"` discriminator
/// would be an independent lock and would let this write land inside a
/// `regenerate` window.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] (exit 74).
pub async fn write_config_json(store: &IndexStore, as_name: &str) -> Result<(), MirrorError> {
    let _lock = store
        .lock_source("index-catalog", as_name, "c/index.json", SOURCE_LOCK_TIMEOUT)
        .await
        .map_err(|error| {
            MirrorError::IndexWriteError(format!(
                "failed to lock source '{as_name}' to write config.json: {error}"
            ))
        })?;

    // Post-lock probe: `lock_source` created the source directory, so no other
    // lock-taking writer can land a config between this and the rename below.
    let target = store.source_config_path(as_name);
    if ocx_lib::utility::fs::path_exists_lossy(&target).await {
        return Ok(());
    }

    let parent = target
        .parent()
        .ok_or_else(|| MirrorError::IndexWriteError(format!("{} has no parent directory", target.display())))?
        .to_path_buf();
    let bytes = serialize_config(&IndexFormatConfig {
        format_version: SUPPORTED_FORMAT_VERSION,
        name_segments: None,
    });

    // `tempfile` and `persist_temp_file` are both blocking.
    let written = target.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
        std::io::Write::write_all(&mut temporary, &bytes)?;
        temporary.as_file().sync_data()?;
        ocx_lib::utility::fs::persist_temp_file(temporary, &written)
    })
    .await
    .map_err(|error| MirrorError::IndexWriteError(format!("the config.json write task panicked: {error}")))?
    .map_err(|error| MirrorError::IndexWriteError(format!("failed to write {}: {error}", target.display())))
}

/// Whether a package can be skipped entirely (C-032).
///
/// Skipped iff **all three** hold:
///
/// 1. its root exists under `p/<ns>/<pkg>.json`; **and**
/// 2. every source `tags{}` key is present locally **with the same `content`
///    digest** — key→digest **pairs**, never key sets; **and**
/// 3. its repository is a key in the local `c/index.json` **and** that entry's
///    digest equals `sha256(local root bytes)`.
///
/// Anything else re-copies. **Pairs, not key sets**, is the guard on the
/// headline requirement: a tag that moves to a new digest without adding a key
/// (a cascade repair, a yank-driven `latest` re-point, a re-push) leaves the
/// key sets identical, so a key-set comparison skips the package **forever**
/// and the mirror serves a stale digest silently — and the tag union makes that
/// permanent, since after any union `local ⊇ source` holds unconditionally.
///
/// `local_root` must be read with `IndexStore::read_root_uncatalogued`, never
/// `read_root`: on a root/catalog straddle `read_root` opens its own
/// `begin_catalog_transaction` on the same key — a self-deadlock that blocks
/// for the lock timeout, errors, and has its error swallowed as best-effort.
/// The straddle condition *is* this predicate's damage list.
pub fn should_skip(
    name: &str,
    source_root: &IndexRoot,
    local_root: Option<&RootReadResult>,
    catalog: &CatalogIndex,
) -> bool {
    // 1. the root exists.
    let Some(local) = local_root else {
        return false;
    };

    // 2. key→digest PAIRS, never key sets. Both maps are already in memory.
    let pairs_match = source_root.tags.iter().all(|(tag, source_tag)| {
        local
            .root
            .tags
            .get(tag)
            .is_some_and(|local_tag| local_tag.content == source_tag.content)
    });
    if !pairs_match {
        return false;
    }

    // 3. the catalog entry is exactly `sha256(local root bytes)`.
    catalog
        .get(name)
        .is_some_and(|entry| *entry == IndexStore::root_catalog_entry(&local.bytes))
}

/// Re-derive `c/index.json` from the roots on disk (C-034).
///
/// Behind `--repair-catalog` only. It repairs catalog↔root disagreement in both
/// directions and nothing else: it re-derives entries from **unchanged root
/// bytes already on disk** and writes no other path — which is also why it
/// cannot undo the tag merge. It is **wholesale**, taking every root under
/// `p/` including ones the current filter excludes, which is why it is a flag
/// and not part of the default run.
///
/// `as_name` is the `source` parameter: the index-source name slugified to a
/// subtree, the same key `begin_catalog_transaction` takes.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] (exit 74).
pub async fn repair_catalog(store: &IndexStore, as_name: &str) -> Result<RegenerateOutcome, MirrorError> {
    regenerate_catalog(store, as_name).await.map_err(|error| {
        MirrorError::IndexWriteError(format!(
            "failed to regenerate the catalog for source '{as_name}': {error}"
        ))
    })
}

#[cfg(test)]
#[path = "index_write/tests.rs"]
mod tests;
