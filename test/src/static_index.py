"""Hand-authored, published-shape ocx-index trees for `registry sync` fixtures.

Ground truth for the wire shapes: `IndexRoot`, `RootTag`, `CatalogDocument`,
`IndexFormatConfig` in ocx's `crates/ocx_lib/src/oci/index/wire.rs` (vendored
at `external/ocx`). Only the "published" shapes are written here:

    config.json                        {"format_version": 1}
    c/index.json                       {"format_version": 1, "packages": {"<ns>/<pkg>": "sha256:<root-digest>", ...}}
    p/<ns>/<pkg>.json                  root: {"repository": "oci://...", "tags": {tag: {"content": "sha256:...", "observed": ...}}}
    p/<ns>/<pkg>/o/<algo>/<hex>.json   the OCI image index a tag resolves to, verbatim

`ocx index sync` against a plain OCI registry produces a *derived* tree with
no `config.json`/`c/index.json` — the mirror's source read requires both, so
a derived tree cannot stand in for this fixture.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
from pathlib import Path


@dataclasses.dataclass(slots=True)
class TreeTag:
    """One `tags{}` entry: a tag name and the OCI image-index digest (`"sha256:<hex>"`) it resolves to."""

    name: str
    content_digest: str
    observed: str = "2026-01-01T00:00:00Z"


@dataclasses.dataclass(slots=True)
class TreePackage:
    """One `p/<name>.json` root to author, plus the dispatch objects its tags point at.

    `name` is the catalog key (`<ns>/<pkg>`). `dispatch_objects` maps each
    referenced digest (`"sha256:<hex>"`) to the verbatim OCI image-index bytes
    served for it — normally the exact bytes a real registry answered for
    that digest (`src.helpers.fetch_manifest`), so a mirror copying by digest
    sees identical content whichever side it reads from.
    """

    name: str
    physical_repository: str
    tags: list[TreeTag]
    dispatch_objects: dict[str, bytes]
    # Package-level human lane, present on every real published root
    # (`kitware/cmake` carries `digest`/`logo`/`readme`). Written when given so
    # fixtures can pin C-028's verbatim passthrough of fields the mirror does
    # not model.
    desc: dict | None = None


def write_published_index_tree(fixture_root: Path, packages: list[TreePackage]) -> None:
    """Hand-authors a published-shape index tree under `fixture_root`.

    Writes `config.json`, one `p/<name>.json` root plus its
    `o/<algo>/<hex>.json` dispatch objects per package, and `c/index.json`
    with each package's entry set to `sha256` of the exact root bytes this
    call writes — the tree is self-consistent by construction
    (`verify_catalog_digests` proves it, and proves a corrupted tree is
    caught).
    """
    fixture_root.mkdir(parents=True, exist_ok=True)
    (fixture_root / "config.json").write_text(json.dumps({"format_version": 1}))

    catalog: dict[str, str] = {}
    for package in packages:
        root = {
            "repository": package.physical_repository,
            "tags": {tag.name: {"content": tag.content_digest, "observed": tag.observed} for tag in package.tags},
        }
        if package.desc is not None:
            root["desc"] = package.desc
        root_bytes = json.dumps(root, sort_keys=True, separators=(",", ":")).encode()
        catalog[package.name] = f"sha256:{hashlib.sha256(root_bytes).hexdigest()}"

        root_path = fixture_root / "p" / f"{package.name}.json"
        root_path.parent.mkdir(parents=True, exist_ok=True)
        root_path.write_bytes(root_bytes)

        for digest, body in package.dispatch_objects.items():
            algo, _, hex_digest = digest.partition(":")
            dispatch_path = fixture_root / "p" / package.name / "o" / algo / f"{hex_digest}.json"
            dispatch_path.parent.mkdir(parents=True, exist_ok=True)
            dispatch_path.write_bytes(body)

    catalog_path = fixture_root / "c" / "index.json"
    catalog_path.parent.mkdir(parents=True, exist_ok=True)
    catalog_document = {"format_version": 1, "packages": catalog}
    catalog_path.write_text(json.dumps(catalog_document, sort_keys=True, separators=(",", ":")))


def verify_catalog_digests(fixture_root: Path) -> list[str]:
    """Returns one message per catalog entry whose digest disagrees with its root document's actual bytes.

    Empty when the tree is self-consistent. This is the same cross-check a
    source read of the tree performs (`c/index.json`'s `packages[name]` must
    equal `sha256` of the root bytes) — used to demonstrate
    `write_published_index_tree` both green (no mismatches) and red (a
    corrupted entry is caught).
    """
    catalog = json.loads((fixture_root / "c" / "index.json").read_text())["packages"]
    mismatches = []
    for name, claimed in catalog.items():
        root_path = fixture_root / "p" / f"{name}.json"
        if not root_path.is_file():
            mismatches.append(f"{name}: catalog entry names a root that does not exist ({root_path})")
            continue
        actual = f"sha256:{hashlib.sha256(root_path.read_bytes()).hexdigest()}"
        if actual != claimed:
            mismatches.append(f"{name}: catalog claims {claimed}, root bytes hash to {actual}")
    return mismatches


# ---------------------------------------------------------------------------
# Produced-tree assertions (WP-16 mechanism): reads a `registry sync` run's
# `output:` tree — same wire shape as the fixtures above, opposite direction.
# Each returns a list of problem messages (empty = property holds), the same
# idiom as `verify_catalog_digests`, so a scenario asserts `== []` or folds
# several calls together before asserting once.
# ---------------------------------------------------------------------------


def verify_root_repository(tree_root: Path, package: str, expected_repository: str) -> list[str]:
    """Checks that `package`'s root document exists and its `repository` field is the rewritten pointer.

    The rewrite is the whole point of a mirror's index write (C-028) — a root
    that still names the *source* repository is silent data corruption, not
    a missing feature.
    """
    root_path = tree_root / "p" / f"{package}.json"
    if not root_path.is_file():
        return [f"{package}: no root document at {root_path}"]
    repository = json.loads(root_path.read_text()).get("repository")
    if repository != expected_repository:
        return [f"{package}: root repository is {repository!r}, expected {expected_repository!r}"]
    return []


def verify_tag_content(tree_root: Path, package: str, tag: str, expected_content_digest: str) -> list[str]:
    """Checks that `package`'s root has `tag` pointing at exactly `expected_content_digest`.

    Checks the key→digest *pair*, not just that the key exists — a re-pointed
    tag (S-027's failure mode) keeps the right key and gets the wrong digest,
    so a key-only presence check would pass right through it.
    """
    root_path = tree_root / "p" / f"{package}.json"
    if not root_path.is_file():
        return [f"{package}: no root document at {root_path}"]
    tags = json.loads(root_path.read_text()).get("tags", {})
    if tag not in tags:
        return [f"{package}: tag {tag!r} missing from root tags {sorted(tags)}"]
    actual = tags[tag].get("content")
    if actual != expected_content_digest:
        return [f"{package}: tag {tag!r} points at {actual!r}, expected {expected_content_digest!r}"]
    return []


def verify_dispatch_object_exists(tree_root: Path, package: str, content_digest: str) -> list[str]:
    """Checks that a dispatch object exists on disk for `content_digest` under `package`'s `o/` subtree."""
    algorithm, _, hex_digest = content_digest.partition(":")
    dispatch_path = tree_root / "p" / package / "o" / algorithm / f"{hex_digest}.json"
    if not dispatch_path.is_file():
        return [f"{package}: no dispatch object at {dispatch_path}"]
    return []


def verify_config_exists(tree_root: Path) -> list[str]:
    """Checks that `config.json` exists directly under `tree_root`."""
    if not (tree_root / "config.json").is_file():
        return [f"no config.json under {tree_root}"]
    return []
