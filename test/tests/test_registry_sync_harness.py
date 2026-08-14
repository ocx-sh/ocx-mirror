"""Self-check for the `registry sync` acceptance-test harness (WP-07, WP-16 mechanism).

Not `test_registry_sync.py` — that file (WP-16) owns the actual `registry
sync` scenarios, once the CLI verb exists. This file proves the *mechanisms*
those scenarios will depend on:

- the second `mirror_registry` docker-compose service is independently
  reachable, and `write_published_index_tree` produces a self-consistent,
  servable published-shape ocx-index tree (WP-07);
- `put_manifest` can seed a descriptor shape `ocx` itself never produces —
  an attestation/referrer child with no `platform` key (S-024);
- the produced-tree assertion helpers (`verify_root_repository`,
  `verify_tag_content`, `verify_dispatch_object_exists`,
  `verify_config_exists`) each catch the specific violation they exist for,
  not just pass on an already-good tree;
- `write_registry_spec` emits the exact `RegistrySpec` shape, with
  `trusted_hosts` empty by default (the SSRF guard stays live unless a
  scenario opts a host in).

Each shown both green and red.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from src.helpers import (
    fetch_manifest,
    push_stub_ocx_package,
    put_manifest,
    registry_is_reachable,
)
from src.registry_spec import SourceSpec, write_registry_spec
from src.static_index import (
    TreePackage,
    TreeTag,
    verify_catalog_digests,
    verify_config_exists,
    verify_dispatch_object_exists,
    verify_root_repository,
    verify_tag_content,
    write_published_index_tree,
)


def test_both_registries_answer_v2_independently(registry: str, mirror_registry: str) -> None:
    """The source and destination docker-compose registries are distinct, independently reachable services."""
    assert registry != mirror_registry, "source and destination fixtures must be different registries"
    assert registry_is_reachable(registry), f"source registry {registry} did not answer /v2/"
    assert registry_is_reachable(mirror_registry), f"destination registry {mirror_registry} did not answer /v2/"


def test_published_index_tree_is_self_consistent_and_servable(
    ocx_binary: Path,
    registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """Green: a tree built from real registry content round-trips over HTTP with matching digests."""
    repository = f"testns/{unique_mirror_repo}"
    push_stub_ocx_package(ocx_binary, registry, f"{repository}:1.0.0", tmp_path / "push-setup")
    digest, body = fetch_manifest(registry, repository, "1.0.0")

    package = TreePackage(
        name=repository,
        physical_repository=f"oci://{registry}/{repository}",
        tags=[TreeTag(name="1.0.0", content_digest=digest)],
        dispatch_objects={digest: body},
    )
    write_published_index_tree(published_index_server.dir, [package])

    # Self-consistency: the catalog's digest for this package matches the
    # exact bytes written for its root document.
    assert verify_catalog_digests(published_index_server.dir) == []

    # Servable: every file a source read needs round-trips over HTTP with the
    # bytes written to disk.
    with urllib.request.urlopen(published_index_server.url("config.json")) as response:
        assert json.loads(response.read()) == {"format_version": 1}

    with urllib.request.urlopen(published_index_server.url("c/index.json")) as response:
        catalog = json.loads(response.read())
    assert catalog["format_version"] == 1
    assert repository in catalog["packages"]

    with urllib.request.urlopen(published_index_server.url(f"p/{repository}.json")) as response:
        root = json.loads(response.read())
    assert root["repository"] == package.physical_repository
    assert root["tags"]["1.0.0"]["content"] == digest

    algo, _, hex_digest = digest.partition(":")
    with urllib.request.urlopen(published_index_server.url(f"p/{repository}/o/{algo}/{hex_digest}.json")) as response:
        dispatch_bytes = response.read()
    assert dispatch_bytes == body, "the dispatch object must be the registry's own manifest bytes, verbatim"


def test_corrupted_catalog_digest_is_caught(tmp_path: Path) -> None:
    """Red: `verify_catalog_digests` must catch a catalog entry whose digest disagrees with its root."""
    fixture_root = tmp_path / "tree"
    placeholder_digest = f"sha256:{'a' * 64}"
    package = TreePackage(
        name="testns/pkg",
        physical_repository="oci://example.invalid/testns/pkg",
        tags=[TreeTag(name="1.0.0", content_digest=placeholder_digest)],
        dispatch_objects={placeholder_digest: b'{"schemaVersion":2}'},
    )
    write_published_index_tree(fixture_root, [package])
    assert verify_catalog_digests(fixture_root) == [], "a freshly written tree must start self-consistent"

    catalog_path = fixture_root / "c" / "index.json"
    catalog = json.loads(catalog_path.read_text())
    catalog["packages"]["testns/pkg"] = f"sha256:{'0' * 64}"
    catalog_path.write_text(json.dumps(catalog))

    mismatches = verify_catalog_digests(fixture_root)
    assert len(mismatches) == 1
    assert "testns/pkg" in mismatches[0]


# ---------------------------------------------------------------------------
# put_manifest (WP-16 mechanism)
# ---------------------------------------------------------------------------


def test_put_manifest_lands_a_descriptor_with_no_platform_key(
    ocx_binary: Path,
    registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """Green: `put_manifest` lands an index descriptor with no `platform` key (S-024's shape),
    referencing a real child manifest, and a subsequent fetch reads it back unchanged."""
    repository = f"testns/{unique_mirror_repo}"
    push_stub_ocx_package(ocx_binary, registry, f"{repository}:1.0.0", tmp_path / "push-setup")
    _, top_body = fetch_manifest(registry, repository, "1.0.0")
    child = json.loads(top_body)["manifests"][0]
    assert "platform" in child, "a real ocx push must carry a platform key -- otherwise this isn't a control"

    index = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            # Same real, already-pushed child digest as the control above, but
            # with no "platform" key -- the attestation/referrer shape.
            {"mediaType": child["mediaType"], "digest": child["digest"], "size": child["size"]}
        ],
    }
    pushed_digest = put_manifest(
        registry,
        repository,
        "no-platform",
        json.dumps(index).encode(),
        "application/vnd.oci.image.index.v1+json",
    )
    assert pushed_digest

    fetched_digest, fetched_body = fetch_manifest(registry, repository, "no-platform")
    assert fetched_digest == pushed_digest
    fetched = json.loads(fetched_body)
    assert "platform" not in fetched["manifests"][0], "the descriptor must round-trip with no platform key"


def test_put_manifest_rejects_a_malformed_body(registry: str, unique_mirror_repo: str) -> None:
    """Red: a non-manifest body is rejected by the registry, surfaced as `urllib.error.HTTPError`."""
    repository = f"testns/{unique_mirror_repo}"
    with pytest.raises(urllib.error.HTTPError):
        put_manifest(
            registry,
            repository,
            "malformed",
            b"not a manifest",
            "application/vnd.oci.image.index.v1+json",
        )


# ---------------------------------------------------------------------------
# Produced-tree assertion helpers (WP-16 mechanism)
# ---------------------------------------------------------------------------


def _one_package_tree(tmp_path: Path, *, repository: str, tag: str, digest: str) -> Path:
    tree_root = tmp_path / "tree"
    package = TreePackage(
        name="testns/pkg",
        physical_repository=repository,
        tags=[TreeTag(name=tag, content_digest=digest)],
        dispatch_objects={digest: b'{"schemaVersion":2}'},
    )
    write_published_index_tree(tree_root, [package])
    return tree_root


def test_verify_root_repository_catches_a_rewrite_that_never_happened(tmp_path: Path) -> None:
    """Green: a root whose `repository` is the rewritten pointer passes. Red: a root still naming
    the source repository (C-028's rewrite skipped) is caught."""
    digest = f"sha256:{'a' * 64}"
    tree_root = _one_package_tree(tmp_path, repository="oci://mirror.example/testns/pkg", tag="1.0.0", digest=digest)

    assert verify_root_repository(tree_root, "testns/pkg", "oci://mirror.example/testns/pkg") == []

    mismatches = verify_root_repository(tree_root, "testns/pkg", "oci://a-different-mirror.example/testns/pkg")
    assert len(mismatches) == 1
    assert "testns/pkg" in mismatches[0]


def test_verify_tag_content_catches_a_repointed_tag(tmp_path: Path) -> None:
    """Green: a tag pointing at the expected digest passes. Red (S-027's failure mode): the same tag
    *key* present but pointing at a stale digest is caught -- a key-only check would miss it."""
    digest_y = f"sha256:{'b' * 64}"
    tree_root = _one_package_tree(
        tmp_path, repository="oci://mirror.example/testns/pkg", tag="latest", digest=digest_y
    )

    assert verify_tag_content(tree_root, "testns/pkg", "latest", digest_y) == []

    digest_x_stale = f"sha256:{'a' * 64}"
    mismatches = verify_tag_content(tree_root, "testns/pkg", "latest", digest_x_stale)
    assert len(mismatches) == 1
    assert "latest" in mismatches[0]


def test_verify_dispatch_object_exists_catches_a_missing_object(tmp_path: Path) -> None:
    """Green: a dispatch object written alongside its tag's digest is found. Red: deleting it (the
    dispatch-before-root ordering, C-030's second invariant, violated) is caught."""
    digest = f"sha256:{'c' * 64}"
    tree_root = _one_package_tree(tmp_path, repository="oci://mirror.example/testns/pkg", tag="1.0.0", digest=digest)

    assert verify_dispatch_object_exists(tree_root, "testns/pkg", digest) == []

    (tree_root / "p" / "testns" / "pkg" / "o" / "sha256" / f"{'c' * 64}.json").unlink()
    mismatches = verify_dispatch_object_exists(tree_root, "testns/pkg", digest)
    assert len(mismatches) == 1
    assert "testns/pkg" in mismatches[0]


def test_verify_config_exists_catches_a_missing_config(tmp_path: Path) -> None:
    """Green: a freshly written tree carries `config.json`. Red: removing it is caught."""
    tree_root = tmp_path / "tree"
    write_published_index_tree(tree_root, [])

    assert verify_config_exists(tree_root) == []

    (tree_root / "config.json").unlink()
    mismatches = verify_config_exists(tree_root)
    assert len(mismatches) == 1


# ---------------------------------------------------------------------------
# write_registry_spec (WP-16 mechanism)
# ---------------------------------------------------------------------------


def test_write_registry_spec_produces_the_expected_shape(tmp_path: Path) -> None:
    """The written document carries exactly `RegistrySpec`'s field names, optional fields present only
    when given, and `trusted_hosts` empty by default -- the SSRF guard (S-007) stays live unless a
    source opts a host in. (Separately verified against a real `serde_yaml_ng::from_str`: plain JSON,
    which this writes, parses unchanged -- YAML 1.2 is a JSON superset.)"""
    spec_path = tmp_path / "registry.yml"
    write_registry_spec(
        spec_path,
        target_registry="localhost:5002",
        target_repository="mirror/prefix",
        output=tmp_path / "out",
        sources=[
            SourceSpec(registry="localhost:5001", index="http://localhost:9999/", include=["kitware/*"]),
            SourceSpec(
                registry="localhost:5001",
                index="http://localhost:9998/",
                as_name="pinned",
                trusted_hosts=["localhost:5001"],
            ),
        ],
    )

    document = json.loads(spec_path.read_text())
    assert document["target"] == {"registry": "localhost:5002", "repository": "mirror/prefix"}
    assert document["destination"] == "{namespace}/{package}"
    assert document["on_error"] == "continue"

    unnamed, pinned = document["sources"]
    assert unnamed["include"] == ["kitware/*"]
    assert "as" not in unnamed, "as: must be omitted, not null, when unset"
    assert "trusted_hosts" not in unnamed, "a source that never opted in must not carry a trusted_hosts key"
    assert pinned["as"] == "pinned"
    assert pinned["trusted_hosts"] == ["localhost:5001"]
