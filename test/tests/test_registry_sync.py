"""Acceptance scenarios for `ocx-mirror registry sync` (WP-16).

The blocking gate for the `registry sync` verb: a two-registry run over the
harness WP-07 built — `registry` (`:5001`) plays the upstream source,
`mirror_registry` (`:5002`) the corporate destination, and
`published_index_server` serves a hand-authored published-shape index tree.

Every assertion is on **observable state** — bytes in the destination
registry, files in the produced `output:` tree, the process exit code, the
report the run printed. Never on a log line.

Scenario references (`S-0NN`) and contract references (`C-0NN`) point at
`.claude/state/plans/plan_registry_mirror_sync.md`.

Two harness-wide constants that are not obvious and are load-bearing:

- **`as:` is always set explicitly.** It defaults to `registry:` verbatim, and
  `localhost:5001` carries a `:`, which is not a legal OCI path component — so
  a spec that leaves it unset fails validation (C-002) for a reason that has
  nothing to do with the scenario under test.
- **`trusted_hosts` carries both loopback spellings.** The source client is
  SSRF-guarded (C-046) and the index base URL must be `https` unless its host
  is trusted (C-016); the index server binds `127.0.0.1` and the source
  registry answers as `localhost`. S-007 is the one scenario that deliberately
  does *not* trust the host it is testing.
"""

from __future__ import annotations

import hashlib
import http.server
import json
import re
import subprocess
import threading
import urllib.error
from pathlib import Path

import pytest

from src.helpers import fetch_manifest, push_ocx_description, push_stub_ocx_package, put_manifest
from src.mirror_runner import MirrorRunner
from src.registry_spec import SourceSpec, write_registry_spec
from src.runner import OcxRunner
from src.static_index import (
    TreePackage,
    TreeTag,
    verify_config_exists,
    verify_dispatch_object_exists,
    verify_root_repository,
    verify_tag_content,
    write_published_index_tree,
)

# The source's `as:` — its subtree under `output:` and its `{registry}`
# expansion. A legal single OCI path component, unlike `localhost:5001`.
SOURCE_AS = "upstream"

# `target.repository` — the containment prefix every copied package lands
# under (C-013). Deliberately two segments deep in no test: the prefix itself
# is what a path-escape attempt has to break out of.
TARGET_PREFIX = "mirror"

INDEX_MEDIA_TYPE = "application/vnd.oci.image.index.v1+json"

# A well-formed sha256 that no registry was ever asked to store. Used to make
# one tag's content copy fail deterministically without a fault-injection
# seam: the pull is by digest, so the registry answers an authoritative 404.
ABSENT_DIGEST = f"sha256:{hashlib.sha256(b'never pushed anywhere').hexdigest()}"


# ---------------------------------------------------------------------------
# Fixtures and helpers
# ---------------------------------------------------------------------------


@pytest.fixture()
def sync(mirror_binary: Path, registry: str, mirror_registry: str, tmp_path: Path) -> MirrorRunner:
    """`ocx-mirror` wired for a two-registry run, every durable side effect under `tmp_path`.

    `XDG_CACHE_HOME` is redirected because C-037 puts the source-catalog
    digest file and the index lock directory under it by default — left alone
    the suite would write into the developer's real `~/.cache` and leak the
    short-circuit state (C-039) from one test into the next.
    """
    work_dir = tmp_path / "run"
    work_dir.mkdir()
    runner = MirrorRunner(mirror_binary, registry, work_dir)
    # Source and destination clients are built separately (C-046); both dial
    # plain HTTP here, so both hosts need the opt-in.
    runner.env["OCX_INSECURE_REGISTRIES"] = f"{registry},{mirror_registry}"
    runner.env["XDG_CACHE_HOME"] = str(tmp_path / "cache")
    return runner


def run_sync(runner: MirrorRunner, spec_path: Path, *flags: str) -> subprocess.CompletedProcess[str]:
    """Runs `registry sync <spec>`, never raising — every scenario asserts its own exit code."""
    return runner.run("registry", "sync", str(spec_path), *flags, check=False)


def outcome(result: subprocess.CompletedProcess[str]) -> str:
    """Full process outcome, for assertion messages."""
    return f"rc={result.returncode}\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"


def report_json(result: subprocess.CompletedProcess[str]) -> dict:
    """Parses the `--format json` report off stdout."""
    start = result.stdout.find("{")
    assert start >= 0, f"no JSON report on stdout\n{outcome(result)}"
    return json.loads(result.stdout[start:])


def source_spec(source_registry: str, index_url: str, **kwargs) -> SourceSpec:
    """One `sources[]` entry for the harness's own loopback source (see module docstring)."""
    kwargs.setdefault("as_name", SOURCE_AS)
    kwargs.setdefault("trusted_hosts", ["127.0.0.1", "localhost"])
    return SourceSpec(registry=source_registry, index=index_url, **kwargs)


def seed_version(
    ocx_binary: Path,
    registry: str,
    repository: str,
    tag: str,
    work_dir: Path,
    content: bytes = b"stub",
) -> tuple[str, bytes]:
    """Pushes one real OCX package version and returns `(image-index digest, verbatim bytes)`."""
    push_stub_ocx_package(ocx_binary, registry, f"{repository}:{tag}", work_dir, content=content)
    return fetch_manifest(registry, repository, tag)


def tree_package(
    registry: str,
    name: str,
    tags: dict[str, str],
    dispatch: dict[str, bytes],
    *,
    physical_path: str | None = None,
    desc: dict | None = None,
) -> TreePackage:
    """A source-tree package rooted at the real repository the content was pushed to.

    `physical_path` decouples the catalog key from the repository the bytes
    live in, which is the production shape: `kitware/cmake`'s root names
    `oci://ghcr.io/ocx-contrib/kitware/cmake`. It defaults to `name` because
    most scenarios do not care, and one that does is
    `test_the_destination_is_derived_from_the_catalog_key`.
    """
    return TreePackage(
        name=name,
        physical_repository=f"oci://{registry}/{physical_path or name}",
        tags=[TreeTag(name=tag, content_digest=digest) for tag, digest in tags.items()],
        dispatch_objects=dispatch,
        desc=desc,
    )


def destination_repository(package: str) -> str:
    """Where `package` lands under the default `{namespace}/{package}` template."""
    return f"{TARGET_PREFIX}/{package}"


def destination_pointer(mirror_registry: str, package: str) -> str:
    """The rewritten `repository` pointer a mirrored root must carry (C-014)."""
    return f"oci://{mirror_registry}/{destination_repository(package)}"


def manifest_absent(registry: str, repository: str, reference: str) -> bool:
    """True iff the registry answers an authoritative 404 for `repository:reference`."""
    try:
        fetch_manifest(registry, repository, reference)
    except urllib.error.HTTPError as error:
        return error.code == 404
    return False


def destination_tags(registry: str, repository: str) -> list[str]:
    """Every tag the destination registry lists for `repository`."""
    with urllib.request.urlopen(f"http://{registry}/v2/{repository}/tags/list") as response:
        return json.loads(response.read()).get("tags") or []


def catalog_of(tree: Path) -> dict[str, str]:
    """The produced tree's `c/index.json` package map."""
    return json.loads((tree / "c" / "index.json").read_text())["packages"]


def tree_snapshot(tree: Path) -> dict[str, tuple[bytes, int]]:
    """Every file under `tree` as `relative path -> (bytes, mtime_ns)` (S-002)."""
    return {
        str(path.relative_to(tree)): (path.read_bytes(), path.stat().st_mtime_ns)
        for path in sorted(tree.rglob("*"))
        if path.is_file()
    }


# ---------------------------------------------------------------------------
# S-001 — the cold run
# ---------------------------------------------------------------------------


def test_cold_sync_publishes_every_package_and_writes_a_servable_tree(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-001: every filtered package's content lands at the destination and the tree points at it."""
    package_a = f"testns/{unique_mirror_repo}_a"
    package_b = f"testns/{unique_mirror_repo}_b"
    digest_a, body_a = seed_version(ocx_binary, registry, package_a, "1.0.0", tmp_path / "push-a", b"a")
    digest_b, body_b = seed_version(ocx_binary, registry, package_b, "2.0.0", tmp_path / "push-b", b"b")

    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(registry, package_a, {"1.0.0": digest_a}, {digest_a: body_a}),
            tree_package(registry, package_b, {"2.0.0": digest_b}, {digest_b: body_b}),
        ],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)
    assert "2 total, 2 copied, 0 skipped, 0 failed" in result.stdout, outcome(result)

    # The bytes are really at the destination, under the composed prefix.
    assert fetch_manifest(mirror_registry, destination_repository(package_a), "1.0.0")[0] == digest_a
    assert fetch_manifest(mirror_registry, destination_repository(package_b), "2.0.0")[0] == digest_b

    tree = output / SOURCE_AS
    problems = (
        verify_config_exists(tree)
        + verify_root_repository(tree, package_a, destination_pointer(mirror_registry, package_a))
        + verify_root_repository(tree, package_b, destination_pointer(mirror_registry, package_b))
        + verify_tag_content(tree, package_a, "1.0.0", digest_a)
        + verify_tag_content(tree, package_b, "2.0.0", digest_b)
        + verify_dispatch_object_exists(tree, package_a, digest_a)
        + verify_dispatch_object_exists(tree, package_b, digest_b)
    )
    assert problems == []
    assert set(catalog_of(tree)) == {package_a, package_b}


def test_the_default_preserves_the_upstream_pointer_while_still_copying_the_content(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """`rewrite_pointers` absent — the shipped default — publishes the upstream address.

    The pair of assertions is the whole point, and neither half means anything
    alone: the content must land at the destination exactly as it does under a
    rewrite, *and* the mirrored root must still name the upstream registry. A
    run that skipped the copy would satisfy the pointer assertion, and a run
    that rewrote would satisfy the copy assertion.

    Written with the key omitted rather than set to `false`, because that is
    the document an operator gets from following the reference: the default
    must be preserve without anyone opting in.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
        rewrite_pointers=False,
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    # Copied, byte for byte, to the same place a rewriting run would put it.
    assert fetch_manifest(mirror_registry, destination_repository(package), "1.0.0")[0] == digest

    # And the published root still sends a client to the upstream registry,
    # which is what makes the client-side `[mirrors]` map the thing that
    # redirects it.
    tree = output / SOURCE_AS
    problems = (
        verify_root_repository(tree, package, f"oci://{registry}/{package}")
        + verify_tag_content(tree, package, "1.0.0", digest)
        + verify_dispatch_object_exists(tree, package, digest)
    )
    assert problems == []
    assert destination_pointer(mirror_registry, package) not in (
        tree / "p" / f"{package}.json"
    ).read_text(), "no part of the destination address may leak into a preserved root"


def test_a_preserved_pointer_whose_landing_path_is_unreachable_warns_without_failing(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """An indirected package under the default lands where no `[mirrors]` prefix reaches.

    The destination expands from the catalog key while the preserved pointer
    names the *physical* path, so a client asks the mirror for
    `<prefix>/ocx-contrib/<pkg>` and the copy sits at `<prefix>/testns/<pkg>`.
    No prefix value resolves one to the other, which is precisely the case the
    warning exists for.

    Asserted as a warning and an exit 0 together: downgrading it to a failure
    would abort a run over client-side configuration this tool does not own,
    and dropping it would ship a mirror nobody can pull from, silently.
    """
    package = f"testns/{unique_mirror_repo}"
    physical_path = f"ocx-contrib/{package}"
    digest, body = seed_version(ocx_binary, registry, physical_path, "1.0.0", tmp_path / "push")

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body}, physical_path=physical_path)],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
        rewrite_pointers=False,
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    noise = result.stdout + result.stderr
    assert "path prefix" in noise, f"the unreachable landing path must be reported\n{outcome(result)}"
    # Both halves of the disagreement, so the operator can see which one to fix.
    assert physical_path in noise, outcome(result)
    assert destination_repository(package) in noise, outcome(result)

    # Still copied and still published — a warning, not a refusal.
    assert fetch_manifest(mirror_registry, destination_repository(package), "1.0.0")[0] == digest


def test_the_destination_is_derived_from_the_catalog_key_not_the_physical_repository(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """C-012: the destination expands from the package *name*, never from the root's `repository`.

    The production shape: `kitware/cmake`'s catalog key is `kitware/cmake`
    while its content lives at `oci://ghcr.io/ocx-contrib/kitware/cmake`. A
    fixture where the two agree cannot tell the two readings apart — this one
    lands the content under a different path than the key, so a copy path
    reading the physical pointer would publish to `mirror/ocx-contrib/…`.
    """
    package = f"testns/{unique_mirror_repo}"
    physical_path = f"ocx-contrib/{package}"
    digest, body = seed_version(ocx_binary, registry, physical_path, "3.31.0", tmp_path / "push")

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"3.31.0": digest}, {digest: body}, physical_path=physical_path)],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    assert not manifest_absent(mirror_registry, destination_repository(package), "3.31.0"), (
        f"nothing landed at {destination_repository(package)}, the repository the catalog key expands to"
    )
    assert fetch_manifest(mirror_registry, destination_repository(package), "3.31.0")[0] == digest
    assert manifest_absent(mirror_registry, f"{TARGET_PREFIX}/{physical_path}", "3.31.0"), (
        "the destination must not be derived from the source's physical repository"
    )
    assert verify_root_repository(output / SOURCE_AS, package, destination_pointer(mirror_registry, package)) == []


def test_an_upstream_keyed_destination_lands_the_copy_where_the_mirror_map_looks(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """C-011: `{upstream_repository}` lands the copy under the *upstream* path, not the catalog key.

    The inverse of the test above, and the shape a preserve-pointer mirror
    needs. A preserved root names the upstream reference, so a client resolves
    it through ocx's `[mirrors]` map — which asks the mirror for
    `<path_prefix>/<upstream repository>` and nothing else. Under the default
    template the copy lands under the catalog key instead, where no prefix
    reaches it.

    Expanding it needs the package's root document, which the catalog does not
    carry, so this also covers the deferred half of the plan phase.
    """
    package = f"testns/{unique_mirror_repo}"
    physical_path = f"ocx-contrib/{package}"
    digest, body = seed_version(ocx_binary, registry, physical_path, "3.31.0", tmp_path / "push")

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"3.31.0": digest}, {digest: body}, physical_path=physical_path)],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        destination="{upstream_repository}",
        rewrite_pointers=False,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    landed = f"{TARGET_PREFIX}/{physical_path}"
    assert not manifest_absent(mirror_registry, landed, "3.31.0"), (
        f"nothing landed at {landed}, where `[mirrors]` rewrites the preserved pointer to"
    )
    assert fetch_manifest(mirror_registry, landed, "3.31.0")[0] == digest
    assert manifest_absent(mirror_registry, destination_repository(package), "3.31.0"), (
        "the catalog-key path is what this template exists to avoid"
    )
    assert verify_root_repository(output / SOURCE_AS, package, f"oci://{registry}/{physical_path}") == [], (
        "preserve must republish the upstream pointer the mirror map is keyed on"
    )


# ---------------------------------------------------------------------------
# S-008 / S-009 — the tag lane: verbatim, and never classified
# ---------------------------------------------------------------------------


def test_non_version_tags_are_mirrored_verbatim_and_never_error(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-008: an unparseable tag is copied by digest and the run stays green.

    The build-stamped shapes (`3.31.0_20260731`) are the ones that will
    actually arrive — roughly half of `kitware/cmake`'s 97 live tags — and
    they are the ones most likely to tempt a parser into "close enough to a
    version, let me try". `20260814` is here for C-021 reason 2: `Version::parse`
    takes a bare all-digit tag as a major-only version, which would sort it
    ahead of every dotted release and let it win `latest`. A run that merely
    *warns* does not satisfy the owner requirement — the exit code is the
    requirement.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "3.31.0", tmp_path / "push")

    verbatim_tags = [
        "3.31.0",
        "3.31.0_20260731",
        "3.31.10_20260730",
        "3.31.4_20260730",
        "20260814",
        "nightly",
    ]
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, dict.fromkeys(verbatim_tags, digest), {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, f"an unparseable tag must not fail the run\n{outcome(result)}"
    assert "1 total, 1 copied, 0 skipped, 0 failed" in result.stdout, outcome(result)

    tree = output / SOURCE_AS
    for tag in verbatim_tags:
        assert fetch_manifest(mirror_registry, destination_repository(package), tag)[0] == digest
        assert verify_tag_content(tree, package, tag, digest) == []

    root_tags = json.loads((tree / "p" / f"{package}.json").read_text())["tags"]
    assert sorted(root_tags) == sorted(verbatim_tags), "no tag may be dropped, added or rewritten"


def test_a_multi_tag_cascade_resolves_to_the_same_digests_as_the_source(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-009: `3.31`, `3` and `latest` resolve at the destination to the digests they resolve to upstream.

    The version pair is `3.31.2` / `3.31.10`, copied from the live cmake tag
    map: `3.31.10` is the newer release numerically and the *lower* one
    lexically, so anything that orders these tags as strings puts the wrong
    digest behind `3.31` and `latest`. The two carry different layer content,
    so they land on different manifest digests — without that the alias
    assertions could not fail at all.
    """
    package = f"testns/{unique_mirror_repo}"
    older, older_body = seed_version(ocx_binary, registry, package, "3.31.2", tmp_path / "push-old", b"v3.31.2")
    newer, newer_body = seed_version(ocx_binary, registry, package, "3.31.10", tmp_path / "push-new", b"v3.31.10")
    assert older != newer, "the two versions must differ, or the cascade assertions cannot fail"
    assert "3.31.10" < "3.31.2", "the pair only traps a string sort while this holds"

    cascade = {
        "3.31.2": older,
        "3.31.10": newer,
        "3.31": newer,
        "3": newer,
        "latest": newer,
        # The build-stamped form the source publishes alongside the plain one,
        # pinned to the older release so a parser that mistakes it for a
        # version would also move `3.31`.
        "3.31.2_20260730": older,
    }
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, cascade, {older: older_body, newer: newer_body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    tree = output / SOURCE_AS
    problems: list[str] = []
    for tag, expected in cascade.items():
        assert fetch_manifest(mirror_registry, destination_repository(package), tag)[0] == expected, (
            f"destination tag {tag!r} must resolve to the digest the source resolves it to"
        )
        problems += verify_tag_content(tree, package, tag, expected)
    assert problems == []


def test_publish_tags_false_copies_the_content_and_creates_no_destination_tag(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """`publish_tags: false` pushes by digest only; the index still carries every tag.

    A client resolving through the mirrored index never reads a destination
    tag — the root maps tag to `content` digest and the pull is by digest — so
    the whole tag set stays usable while the registry gains none of it. What
    the destination must still hold is the content itself, addressable by the
    digest the root names.
    """
    package = f"testns/{unique_mirror_repo}"
    older, older_body = seed_version(ocx_binary, registry, package, "3.31.2", tmp_path / "push-old", b"v3.31.2")
    newer, newer_body = seed_version(ocx_binary, registry, package, "3.31.10", tmp_path / "push-new", b"v3.31.10")
    cascade = {"3.31.2": older, "3.31.10": newer, "3.31": newer, "latest": newer}
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, cascade, {older: older_body, newer: newer_body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
        extra={"publish_tags": False},
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    repository = destination_repository(package)
    tree = output / SOURCE_AS
    problems: list[str] = []
    for tag, expected in cascade.items():
        if not manifest_absent(mirror_registry, repository, tag):
            problems.append(f"destination tag {tag!r} exists, but publish_tags is false")
        # The content the root names is what a client actually fetches.
        if fetch_manifest(mirror_registry, repository, expected)[0] != expected:
            problems.append(f"content {expected} for tag {tag!r} is not readable by digest")
        problems += verify_tag_content(tree, package, tag, expected)
    assert problems == []


# ---------------------------------------------------------------------------
# S-024 / S-010 — the copy walk enumerates every descriptor
# ---------------------------------------------------------------------------


def test_a_descriptor_with_no_platform_key_lands_at_the_destination(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-024: an attestation/referrer-shaped child (no `platform`) must be copied, not skipped.

    ocx's platform-candidate walk returns `None` for a descriptor that omits
    `platform`, which is right for *resolution* and wrong for a *mirror*
    (C-022). Every other scenario here passes with that walk in place; this
    one is the only one that catches it.
    """
    package = f"testns/{unique_mirror_repo}"
    _, body = seed_version(ocx_binary, registry, package, "seed", tmp_path / "push")
    child = json.loads(body)["manifests"][0]
    assert "platform" in child, "a real ocx push must carry a platform key -- otherwise this is not a control"

    index = {
        "schemaVersion": 2,
        "mediaType": INDEX_MEDIA_TYPE,
        "manifests": [{"mediaType": child["mediaType"], "digest": child["digest"], "size": child["size"]}],
    }
    index_digest = put_manifest(registry, package, "no-platform", json.dumps(index).encode(), INDEX_MEDIA_TYPE)

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": index_digest}, {index_digest: json.dumps(index).encode()})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    mirrored = destination_repository(package)
    assert fetch_manifest(mirror_registry, mirrored, "1.0.0")[0] == index_digest
    # Presence is checked before the digest comparison so a skipped child reds
    # with the digest in the message rather than an undecorated HTTP 404.
    assert not manifest_absent(mirror_registry, mirrored, child["digest"]), (
        f"the platform-less child {child['digest']} is missing from {mirrored} -- an index "
        "referencing a manifest that was never copied resolves nowhere"
    )
    assert fetch_manifest(mirror_registry, mirrored, child["digest"])[0] == child["digest"]


def test_every_platform_of_a_multi_platform_index_is_copied(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-010: the destination index lists the same descriptor set as the source's, by digest."""
    package = f"testns/{unique_mirror_repo}"
    platforms = [
        ("linux", "amd64", b"linux-amd64"),
        ("linux", "arm64", b"linux-arm64"),
        ("darwin", "arm64", b"darwin-arm64"),
    ]

    descriptors = []
    for index, (operating_system, architecture, content) in enumerate(platforms):
        _, body = seed_version(ocx_binary, registry, package, f"seed{index}", tmp_path / f"push{index}", content)
        child = json.loads(body)["manifests"][0]
        descriptors.append(
            {
                "mediaType": child["mediaType"],
                "digest": child["digest"],
                "size": child["size"],
                "platform": {"os": operating_system, "architecture": architecture},
            }
        )
    assert len({descriptor["digest"] for descriptor in descriptors}) == 3, (
        "the three children must be distinct, or a platform-filtered copy would pass"
    )

    index_bytes = json.dumps({"schemaVersion": 2, "mediaType": INDEX_MEDIA_TYPE, "manifests": descriptors}).encode()
    index_digest = put_manifest(registry, package, "multi", index_bytes, INDEX_MEDIA_TYPE)

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": index_digest}, {index_digest: index_bytes})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)

    mirrored = destination_repository(package)
    _, mirrored_index = fetch_manifest(mirror_registry, mirrored, "1.0.0")
    assert [entry["digest"] for entry in json.loads(mirrored_index)["manifests"]] == [
        descriptor["digest"] for descriptor in descriptors
    ]
    for descriptor in descriptors:
        assert fetch_manifest(mirror_registry, mirrored, descriptor["digest"])[0] == descriptor["digest"]


def test_every_copied_manifest_carries_its_canonical_digest_tag(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """`sha256.<hex>` lands for the index and for every platform manifest under it.

    These are ocx's registry-side deletion safety net: a manifest tagged after
    its own digest cannot be orphaned by a stray delete of a rolling tag. They
    are reserved, so they never appear in an index root's `tags{}` and no
    walk of the published tag set reaches them — a mirror that copies tags
    faithfully creates none of them unless it is told to.

    Shown red as well as green: the same run under `canonical_tags: false`
    must produce the version tag and nothing digest-named.
    """
    package = f"testns/{unique_mirror_repo}"
    descriptors = []
    for index, content in enumerate((b"linux-amd64", b"darwin-arm64")):
        _, body = seed_version(ocx_binary, registry, package, f"seed{index}", tmp_path / f"push{index}", content)
        child = json.loads(body)["manifests"][0]
        descriptors.append({key: child[key] for key in ("mediaType", "digest", "size")})
    index_bytes = json.dumps({"schemaVersion": 2, "mediaType": INDEX_MEDIA_TYPE, "manifests": descriptors}).encode()
    index_digest = put_manifest(registry, package, "multi", index_bytes, INDEX_MEDIA_TYPE)
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": index_digest}, {index_digest: index_bytes})],
    )

    def canonical(digest: str) -> str:
        return digest.replace(":", ".")

    expected = [canonical(index_digest)] + [canonical(descriptor["digest"]) for descriptor in descriptors]

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )
    assert run_sync(sync, spec).returncode == 0

    mirrored = destination_repository(package)
    tags = destination_tags(mirror_registry, mirrored)
    assert set(expected) <= set(tags), f"canonical tags missing: {sorted(set(expected) - set(tags))}"
    for tag, digest in zip(expected, [index_digest] + [d["digest"] for d in descriptors], strict=True):
        assert fetch_manifest(mirror_registry, mirrored, tag)[0] == digest, (
            f"{tag} must resolve to the digest it names"
        )

    # Red half: the same source copied again with the knob off, into a
    # destination of its own so the tags above cannot be mistaken for these.
    off_spec = tmp_path / "registry-off.yml"
    write_registry_spec(
        off_spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=tmp_path / "public-off",
        destination="off/{namespace}/{package}",
        sources=[source_spec(registry, published_index_server.url())],
        extra={"canonical_tags": False},
    )
    assert run_sync(sync, off_spec).returncode == 0

    off_tags = destination_tags(mirror_registry, f"{TARGET_PREFIX}/off/{package}")
    assert off_tags == ["1.0.0"], f"canonical_tags: false must write the version tag alone, got {off_tags}"


# ---------------------------------------------------------------------------
# S-028 — the output tree is created, not required
# ---------------------------------------------------------------------------


def test_an_output_path_that_does_not_exist_is_created(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-028: `output:` naming a path that does not exist yet is created by the run.

    The path is constructed here and never touched, because a pytest tmpdir
    already exists — which is exactly why this gap survived to WP-06.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "does" / "not" / "exist" / "public"
    assert not output.exists(), "the scenario is only meaningful against a path nothing pre-created"

    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, outcome(result)
    assert (output / SOURCE_AS / "config.json").is_file(), outcome(result)
    assert verify_root_repository(output / SOURCE_AS, package, destination_pointer(mirror_registry, package)) == []


# ---------------------------------------------------------------------------
# S-017 / S-026 — failure isolation, the exit code, and the `on_error` policy
# ---------------------------------------------------------------------------


def test_a_failed_package_is_absent_while_its_healthy_sibling_is_published(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-017: one package fails, the run exits non-zero, the others are still published.

    Both halves are the assertion. The invariant under test is that a release
    enters the index only after its content copy succeeded (C-030), so the
    failed package must be absent from the produced tree *and* from the
    catalog — not merely reported.
    """
    healthy = f"testns/{unique_mirror_repo}_good"
    broken = f"testns/{unique_mirror_repo}_bad"
    digest, body = seed_version(ocx_binary, registry, healthy, "1.0.0", tmp_path / "push")

    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(registry, healthy, {"1.0.0": digest}, {digest: body}),
            # The root names a digest the source registry never stored, so the
            # by-digest pull answers an authoritative 404 and fails the package.
            tree_package(registry, broken, {"1.0.0": ABSENT_DIGEST}, {}),
        ],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
        on_error="continue",
    )

    result = run_sync(sync, spec, "--format", "json")
    assert result.returncode != 0, f"a run with a failed package must exit non-zero\n{outcome(result)}"

    report = report_json(result)
    assert report["counters"]["failed"] == 1, report
    assert report["counters"]["copied"] == 1, report

    tree = output / SOURCE_AS
    assert verify_root_repository(tree, healthy, destination_pointer(mirror_registry, healthy)) == []
    assert fetch_manifest(mirror_registry, destination_repository(healthy), "1.0.0")[0] == digest

    assert not (tree / "p" / f"{broken}.json").exists(), "a package whose content never copied must have no root"
    assert broken not in catalog_of(tree), "a package whose content never copied must not enter the catalog"


def test_fail_fast_stops_the_run_where_continue_finishes_it(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-017: the default `continue` policy against the opt-in `--fail-fast`, on one seeded source.

    The failing package sorts before the healthy one (`aaa` / `zzz`) because
    the catalog is a `BTreeMap` on the wire — so under `fail_fast` the healthy
    package is still ahead of the run when it aborts, and under `continue` it
    is reached. If the healthy package turns up under `fail_fast`, either the
    policy did not stop the run or packages are not processed in catalog
    order; both are findings.
    """
    broken = f"testns/aaa_{unique_mirror_repo}"
    healthy = f"testns/zzz_{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, healthy, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(registry, broken, {"1.0.0": ABSENT_DIGEST}, {}),
            tree_package(registry, healthy, {"1.0.0": digest}, {digest: body}),
        ],
    )

    continue_output = tmp_path / "continue"
    continue_spec = tmp_path / "continue.yml"
    write_registry_spec(
        continue_spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=continue_output,
        sources=[source_spec(registry, published_index_server.url())],
        on_error="continue",
    )
    continue_result = run_sync(sync, continue_spec)
    assert continue_result.returncode != 0, outcome(continue_result)
    assert (continue_output / SOURCE_AS / "p" / f"{healthy}.json").is_file(), (
        f"`continue` must reach every package after the failure\n{outcome(continue_result)}"
    )

    fail_fast_output = tmp_path / "fail-fast"
    fail_fast_spec = tmp_path / "fail-fast.yml"
    write_registry_spec(
        fail_fast_spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=fail_fast_output,
        sources=[source_spec(registry, published_index_server.url())],
        on_error="continue",
    )
    fail_fast_result = run_sync(sync, fail_fast_spec, "--fail-fast")
    assert fail_fast_result.returncode != 0, outcome(fail_fast_result)
    assert not (fail_fast_output / SOURCE_AS / "p" / f"{healthy}.json").exists(), (
        f"`--fail-fast` must abort at the first per-package failure\n{outcome(fail_fast_result)}"
    )


def test_a_failed_tag_never_deletes_a_previously_mirrored_tag(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-026: run twice, second run failing tag `B`; `B` survives with its run-1 digest.

    `CatalogTransaction::write_root` is merge-blind, so without the
    mirror-owned tag union (C-047) the second run publishes a root holding
    only what it confirmed — silently deleting a tag a previous run had
    mirrored. Every all-succeed scenario in this file passes either way.
    """
    package = f"testns/{unique_mirror_repo}"
    digest_a, body_a = seed_version(ocx_binary, registry, package, "a", tmp_path / "push-a", b"tag-a")
    digest_b, body_b = seed_version(ocx_binary, registry, package, "b", tmp_path / "push-b", b"tag-b")

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
        on_error="continue",
    )

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"A": digest_a, "B": digest_b}, {digest_a: body_a, digest_b: body_b})],
    )
    first = run_sync(sync, spec)
    assert first.returncode == 0, outcome(first)

    tree = output / SOURCE_AS
    assert verify_tag_content(tree, package, "B", digest_b) == []

    # Run 2: A moves to new content that copies fine, B moves to content the
    # source registry does not hold.
    digest_a2, body_a2 = seed_version(ocx_binary, registry, package, "a2", tmp_path / "push-a2", b"tag-a-moved")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"A": digest_a2, "B": ABSENT_DIGEST}, {digest_a2: body_a2})],
    )
    second = run_sync(sync, spec)
    assert second.returncode != 0, f"a tag whose content cannot be pulled must fail the package\n{outcome(second)}"

    assert verify_tag_content(tree, package, "B", digest_b) == [], (
        "a failed tag must never delete the digest a previous run published"
    )


def test_a_partially_failed_package_still_publishes_the_tags_that_copied(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-026, second half: the tag that *did* copy is updated in the same run that failed another.

    This is the reading C-047 states ("`tags` — a union: destination existing
    tags ∪ this run's confirmed tags, this run winning on conflict") and it
    contradicts the ADR's older "the package is counted failed, no root is
    written". Kept separate from the never-delete assertion above, which holds
    under both readings, so a failure here localises the disagreement.
    """
    package = f"testns/{unique_mirror_repo}"
    digest_a, body_a = seed_version(ocx_binary, registry, package, "a", tmp_path / "push-a", b"tag-a")
    digest_b, body_b = seed_version(ocx_binary, registry, package, "b", tmp_path / "push-b", b"tag-b")

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
        on_error="continue",
    )

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"A": digest_a, "B": digest_b}, {digest_a: body_a, digest_b: body_b})],
    )
    assert run_sync(sync, spec).returncode == 0

    digest_a2, body_a2 = seed_version(ocx_binary, registry, package, "a2", tmp_path / "push-a2", b"tag-a-moved")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"A": digest_a2, "B": ABSENT_DIGEST}, {digest_a2: body_a2})],
    )
    second = run_sync(sync, spec)
    assert second.returncode != 0, outcome(second)

    assert verify_tag_content(output / SOURCE_AS, package, "A", digest_a2) == [], (
        "the tag whose content copied must be published even though a sibling tag failed"
    )


# ---------------------------------------------------------------------------
# S-002 / S-027 / S-016 — the short-circuit and what must defeat it
# ---------------------------------------------------------------------------


def test_a_rerun_with_an_unchanged_source_copies_nothing_and_leaves_the_tree_alone(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-002: a second run over an unchanged source copies nothing and rewrites nothing.

    Shown red as well as green, per the unfalsifiable-green rule: the third
    phase adds a tag upstream and the same assertions must then report a copy
    and a changed tree.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push", b"one")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    first = run_sync(sync, spec)
    assert first.returncode == 0, outcome(first)
    tree = output / SOURCE_AS
    after_first = tree_snapshot(tree)

    second = run_sync(sync, spec)
    assert second.returncode == 0, outcome(second)
    assert "0 copied" in second.stdout, f"a no-op run must still report, and report zero\n{outcome(second)}"
    assert tree_snapshot(tree) == after_first, "an unchanged source must leave the tree byte- and mtime-identical"

    # Red half: one added tag upstream and the same two assertions must flip.
    second_digest, second_body = seed_version(ocx_binary, registry, package, "1.1.0", tmp_path / "push2", b"two")
    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(
                registry,
                package,
                {"1.0.0": digest, "1.1.0": second_digest},
                {digest: body, second_digest: second_body},
            )
        ],
    )
    third = run_sync(sync, spec)
    assert third.returncode == 0, outcome(third)
    assert "1 copied" in third.stdout, f"a changed source must copy\n{outcome(third)}"
    assert tree_snapshot(tree) != after_first, "a changed source must rewrite the tree"
    assert verify_tag_content(tree, package, "1.1.0", second_digest) == []


def test_a_tag_repointed_without_a_new_key_is_recopied(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-027: `latest` moves from X to Y with no new tag key; the destination must resolve Y.

    A key-set comparison sees an unchanged set and skips the package forever,
    serving a stale digest silently — which is why C-032/C-039 compare
    key→digest *pairs*.
    """
    package = f"testns/{unique_mirror_repo}"
    digest_x, body_x = seed_version(ocx_binary, registry, package, "1.2.3", tmp_path / "push-x", b"x")
    digest_y, body_y = seed_version(ocx_binary, registry, package, "1.2.4", tmp_path / "push-y", b"y")

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.2.3": digest_x, "latest": digest_x}, {digest_x: body_x})],
    )
    assert run_sync(sync, spec).returncode == 0

    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(
                registry,
                package,
                {"1.2.3": digest_x, "latest": digest_y},
                {digest_x: body_x, digest_y: body_y},
            )
        ],
    )
    second = run_sync(sync, spec)
    assert second.returncode == 0, outcome(second)

    assert verify_tag_content(output / SOURCE_AS, package, "latest", digest_y) == []
    assert fetch_manifest(mirror_registry, destination_repository(package), "latest")[0] == digest_y


def test_widening_include_copies_the_newly_matched_package(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-016: the source catalog is byte-identical, so only the name-set condition can catch this."""
    first_package = f"testns/{unique_mirror_repo}_first"
    second_package = f"testns/{unique_mirror_repo}_second"
    digest_one, body_one = seed_version(ocx_binary, registry, first_package, "1.0.0", tmp_path / "push-1", b"1")
    digest_two, body_two = seed_version(ocx_binary, registry, second_package, "1.0.0", tmp_path / "push-2", b"2")
    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(registry, first_package, {"1.0.0": digest_one}, {digest_one: body_one}),
            tree_package(registry, second_package, {"1.0.0": digest_two}, {digest_two: body_two}),
        ],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"

    def write_spec(include: list[str]) -> None:
        write_registry_spec(
            spec,
            target_registry=mirror_registry,
            target_repository=TARGET_PREFIX,
            output=output,
            sources=[source_spec(registry, published_index_server.url(), include=include)],
        )

    write_spec([first_package])
    narrow = run_sync(sync, spec)
    assert narrow.returncode == 0, outcome(narrow)
    assert not (output / SOURCE_AS / "p" / f"{second_package}.json").exists()

    write_spec([first_package, second_package])
    widened = run_sync(sync, spec)
    assert widened.returncode == 0, outcome(widened)
    assert verify_root_repository(
        output / SOURCE_AS, second_package, destination_pointer(mirror_registry, second_package)
    ) == [], f"a widened include: must copy the newly matched package\n{outcome(widened)}"


# ---------------------------------------------------------------------------
# Spec-layer refusals — S-003, S-005, S-004, S-006, S-018, S-013
# ---------------------------------------------------------------------------


def test_credentials_anywhere_in_the_spec_are_refused_without_echoing_the_value(
    sync: MirrorRunner,
    registry: str,
    mirror_registry: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-003: a credential key exits 64, names the key path and the env var, and never echoes the value."""
    secret = "hunter2"

    direct = tmp_path / "direct.yml"
    write_registry_spec(
        direct,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=tmp_path / "public",
        sources=[source_spec(registry, published_index_server.url())],
        extra={"password": secret},
    )
    result = run_sync(sync, direct)
    assert result.returncode == 64, outcome(result)
    assert secret not in result.stdout + result.stderr, "the offending value must never be echoed"
    assert "password" in result.stderr + result.stdout, outcome(result)
    assert "OCX_AUTH" in result.stderr + result.stdout, outcome(result)

    base = tmp_path / "base.yml"
    base.write_text(json.dumps({"password": secret}))
    inherited = tmp_path / "inherited.yml"
    write_registry_spec(
        inherited,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=tmp_path / "public",
        sources=[source_spec(registry, published_index_server.url())],
        extra={"extends": "base.yml"},
    )
    inherited_result = run_sync(sync, inherited)
    assert inherited_result.returncode == 64, f"a credential in an extends: base must be caught\n{outcome(inherited_result)}"
    assert secret not in inherited_result.stdout + inherited_result.stderr


def test_userinfo_in_the_index_url_is_refused_without_echoing_it(
    sync: MirrorRunner,
    registry: str,
    mirror_registry: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-005: `https://user:pass@host/` exits 64 and the password appears nowhere."""
    host = published_index_server.base_url.removeprefix("http://")
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=tmp_path / "public",
        sources=[source_spec(registry, f"http://operator:hunter2@{host}/")],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 64, outcome(result)
    assert "hunter2" not in result.stdout + result.stderr, "the password must never reach stdout or stderr"
    assert "index" in result.stdout + result.stderr, outcome(result)


def test_the_registry_placeholder_is_optional_for_one_source_and_mandatory_for_two(
    sync: MirrorRunner,
    registry: str,
    mirror_registry: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-004: the same `destination:` is valid with one source and invalid with two."""
    write_published_index_tree(published_index_server.dir, [])

    single = tmp_path / "single.yml"
    write_registry_spec(
        single,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=tmp_path / "single-out",
        sources=[source_spec(registry, published_index_server.url())],
        destination="{namespace}/{package}",
    )
    single_result = run_sync(sync, single)
    assert single_result.returncode == 0, outcome(single_result)

    double = tmp_path / "double.yml"
    write_registry_spec(
        double,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=tmp_path / "double-out",
        sources=[
            source_spec(registry, published_index_server.url(), as_name="first"),
            source_spec(registry, published_index_server.url(), as_name="second"),
        ],
        destination="{namespace}/{package}",
    )
    double_result = run_sync(sync, double)
    assert double_result.returncode == 65, outcome(double_result)
    assert "{registry}" in double_result.stdout + double_result.stderr, outcome(double_result)


@pytest.mark.parametrize(
    "hostile_key",
    [
        pytest.param("foo/../../prod-images", id="traversal"),
        pytest.param("Foo/Bar", id="uppercase"),
    ],
)
def test_a_hostile_catalog_key_is_refused_at_plan_time(
    hostile_key: str,
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-006: a catalog key that would escape or fold the destination prefix is refused, never repaired.

    The key is injected into `c/index.json` after the tree is written, so the
    fixture writer never has to create a path out of it — the mirror refuses
    at plan time, before any root is fetched.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )
    catalog_path = published_index_server.dir / "c" / "index.json"
    catalog = json.loads(catalog_path.read_text())
    catalog["packages"][hostile_key] = f"sha256:{'0' * 64}"
    catalog_path.write_text(json.dumps(catalog))

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 65, outcome(result)
    assert not list(tmp_path.glob("prod-images*")), "nothing may be written outside the configured prefix"
    assert not (output / SOURCE_AS / "p").exists(), "a plan-time refusal must write no roots at all"


def test_two_packages_expanding_to_one_destination_are_refused(
    sync: MirrorRunner,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-018: a destination collision is refused at plan time, naming both keys."""
    first = f"nsa/{unique_mirror_repo}"
    second = f"nsb/{unique_mirror_repo}"
    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(registry, first, {"1.0.0": ABSENT_DIGEST}, {}),
            tree_package(registry, second, {"1.0.0": ABSENT_DIGEST}, {}),
        ],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
        # Drops `{namespace}`, so two namespaces' same-named packages collide.
        destination="{package}",
    )

    result = run_sync(sync, spec)
    assert result.returncode == 65, outcome(result)
    message = result.stdout + result.stderr
    assert first in message and second in message, f"the refusal must name both keys\n{outcome(result)}"
    assert not (output / SOURCE_AS / "p").exists(), "a collision is refused before anything is written"


def test_a_source_declaring_an_unsupported_format_version_writes_nothing(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-013: `format_version: 2` exits 65 and the run writes nothing."""
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )
    (published_index_server.dir / "config.json").write_text(json.dumps({"format_version": 2}))

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 65, outcome(result)
    assert not (output / SOURCE_AS / "config.json").exists(), "an unsupported source format must write nothing"
    assert not (output / SOURCE_AS / "p").exists()


# ---------------------------------------------------------------------------
# S-007 — the SSRF floor on a root's physical host
# ---------------------------------------------------------------------------


def test_a_root_naming_a_forbidden_physical_host_is_refused(
    sync: MirrorRunner,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-007: a root pointing at the metadata service is refused with 69 and nothing is written.

    This spec cannot be shared with any other scenario, and it is not the
    empty-`trusted_hosts` spec the plan sketched: the index base URL is plain
    `http` on loopback, so with nothing trusted the run would be refused at
    load, before a root was ever read, and the guard under test would never
    run. So the *index* host is trusted and the *physical* host is not —
    which is the shape the guard actually defends (foreign data steering the
    mirror at a link-local address).

    The recorder proves the run reached the root document: "zero registry
    requests" on its own is equally consistent with nothing having run.
    """
    package = f"testns/{unique_mirror_repo}"
    write_published_index_tree(
        published_index_server.dir,
        [
            TreePackage(
                name=package,
                physical_repository="oci://169.254.169.254/x/y",
                tags=[TreeTag(name="1.0.0", content_digest=ABSENT_DIGEST)],
                dispatch_objects={},
            )
        ],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url(), trusted_hosts=["127.0.0.1"])],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 69, outcome(result)
    assert any(f"/p/{package}.json" in request for request in published_index_server.requests), (
        f"the run must have read the root before refusing it\n{published_index_server.requests}"
    )
    assert not (output / SOURCE_AS / "p" / f"{package}.json").exists()


# ---------------------------------------------------------------------------
# S-019 / S-020 — dry run, and what may exist under `output:`
# ---------------------------------------------------------------------------


def test_dry_run_copies_nothing_and_estimates_what_a_real_run_would_transfer(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-019: `--dry-run` reports bytes and writes nothing; after a real run the estimate is zero."""
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    cold = run_sync(sync, spec, "--dry-run", "--format", "json")
    assert cold.returncode == 0, outcome(cold)
    assert report_json(cold)["estimated_bytes"] > 0, report_json(cold)
    assert manifest_absent(mirror_registry, destination_repository(package), "1.0.0"), (
        "--dry-run must copy nothing"
    )
    assert not (output / SOURCE_AS / "p").exists(), "--dry-run must write no index tree"
    assert not list((tmp_path / "cache").rglob("*.digest")), "--dry-run must not record the catalog digest"

    real = run_sync(sync, spec)
    assert real.returncode == 0, outcome(real)

    warm = run_sync(sync, spec, "--dry-run", "--format", "json")
    assert warm.returncode == 0, outcome(warm)
    assert report_json(warm)["estimated_bytes"] == 0, (
        f"nothing is left to transfer once the content is at the destination\n{outcome(warm)}"
    )


def test_the_output_tree_contains_only_wire_content(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-020: no `locks/`, no cache file, no `.etag` — asserted on the recursive listing, not a denylist."""
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )
    assert run_sync(sync, spec).returncode == 0

    allowed = re.compile(rf"^{re.escape(SOURCE_AS)}/(config\.json|c/index\.json|p/.+)$")
    listing = sorted(path.relative_to(output).as_posix() for path in output.rglob("*") if path.is_file())
    assert listing, "the run must have written something"
    unexpected = [entry for entry in listing if not allowed.match(entry)]
    assert unexpected == [], f"only wire content may live under output:\nfull listing: {listing}"


# ---------------------------------------------------------------------------
# S-014 / S-015 / S-021 — repair
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("damage", ["missing", "corrupted"])
def test_a_catalog_entry_that_disagrees_with_its_root_is_republished(
    damage: str,
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-015: an entry deleted from — or corrupted in — `c/index.json` is restored by the next ordinary run."""
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )
    assert run_sync(sync, spec).returncode == 0

    tree = output / SOURCE_AS
    catalog_path = tree / "c" / "index.json"
    catalog = json.loads(catalog_path.read_text())
    if damage == "missing":
        del catalog["packages"][package]
    else:
        catalog["packages"][package] = f"sha256:{'0' * 64}"
    catalog_path.write_text(json.dumps(catalog))

    repaired = run_sync(sync, spec)
    assert repaired.returncode == 0, outcome(repaired)

    root_bytes = (tree / "p" / f"{package}.json").read_bytes()
    expected = f"sha256:{hashlib.sha256(root_bytes).hexdigest()}"
    assert catalog_of(tree).get(package) == expected, (
        "the next run must republish the catalog entry from the root on disk (S-015). "
        "A `corrupted` failure here is C-039's short-circuit masking C-032's third condition: "
        "the entry's KEY is still present, so the name-set subset test passes, the source catalog "
        "digest is unchanged, the source pass is skipped whole, and the per-package predicate that "
        "compares the entry against sha256(root bytes) is never reached. The `missing` case passes "
        f"only because deleting the key breaks the subset test.\n{outcome(repaired)}"
    )


def test_a_drifted_catalog_entry_is_repaired_once_the_short_circuit_cannot_fire(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """C-032 condition 3, isolated from C-039's short-circuit.

    Same damage as the `corrupted` case above, but a *second* package moves
    upstream so the source-catalog digest changes and the short-circuit cannot
    fire. This separates the two candidate causes: if this passes and the
    parametrized case fails, the skip predicate is right and the short-circuit
    is what hides it; if both fail, condition 3 is not implemented at all.
    """
    drifted = f"testns/{unique_mirror_repo}_drifted"
    mover = f"testns/{unique_mirror_repo}_mover"
    drifted_digest, drifted_body = seed_version(ocx_binary, registry, drifted, "1.0.0", tmp_path / "push-d", b"d")
    mover_digest, mover_body = seed_version(ocx_binary, registry, mover, "1.0.0", tmp_path / "push-m", b"m")

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(registry, drifted, {"1.0.0": drifted_digest}, {drifted_digest: drifted_body}),
            tree_package(registry, mover, {"1.0.0": mover_digest}, {mover_digest: mover_body}),
        ],
    )
    assert run_sync(sync, spec).returncode == 0

    tree = output / SOURCE_AS
    catalog_path = tree / "c" / "index.json"
    catalog = json.loads(catalog_path.read_text())
    catalog["packages"][drifted] = f"sha256:{'0' * 64}"
    catalog_path.write_text(json.dumps(catalog))

    moved_digest, moved_body = seed_version(ocx_binary, registry, mover, "1.1.0", tmp_path / "push-m2", b"m2")
    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(registry, drifted, {"1.0.0": drifted_digest}, {drifted_digest: drifted_body}),
            tree_package(
                registry,
                mover,
                {"1.0.0": mover_digest, "1.1.0": moved_digest},
                {mover_digest: mover_body, moved_digest: moved_body},
            ),
        ],
    )
    second = run_sync(sync, spec)
    assert second.returncode == 0, outcome(second)

    root_bytes = (tree / "p" / f"{drifted}.json").read_bytes()
    expected = f"sha256:{hashlib.sha256(root_bytes).hexdigest()}"
    assert catalog_of(tree).get(drifted) == expected, (
        f"a catalog entry that disagrees with its root must be re-derived (C-032 condition 3)\n{outcome(second)}"
    )


def test_a_root_deleted_after_its_content_landed_is_written_again(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-014: the interrupted state (content at the destination, no root) is repaired by the next run.

    A real SIGINT mid-package is not reproducible deterministically; deleting
    the root while leaving the destination content in place reproduces the
    damage state C-032 row 1 describes, which is what the predicate has to
    detect.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )
    assert run_sync(sync, spec).returncode == 0

    tree = output / SOURCE_AS
    (tree / "p" / f"{package}.json").unlink()
    catalog_path = tree / "c" / "index.json"
    catalog = json.loads(catalog_path.read_text())
    del catalog["packages"][package]
    catalog_path.write_text(json.dumps(catalog))

    resumed = run_sync(sync, spec)
    assert resumed.returncode == 0, outcome(resumed)
    assert verify_root_repository(tree, package, destination_pointer(mirror_registry, package)) == []
    assert verify_tag_content(tree, package, "1.0.0", digest) == []
    assert package in catalog_of(tree)


def test_repair_catalog_drops_an_entry_whose_root_is_gone(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-021: `--repair-catalog` re-derives `c/index.json` from the roots on disk; a plain run does not."""
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )
    assert run_sync(sync, spec).returncode == 0

    tree = output / SOURCE_AS
    catalog_path = tree / "c" / "index.json"
    catalog = json.loads(catalog_path.read_text())
    catalog["packages"]["testns/ghost"] = f"sha256:{'0' * 64}"
    catalog_path.write_text(json.dumps(catalog))

    plain = run_sync(sync, spec)
    assert plain.returncode == 0, outcome(plain)
    assert "testns/ghost" in catalog_of(tree), (
        "an ordinary run must not reconcile the whole catalog -- that is what the flag is for"
    )

    repaired = run_sync(sync, spec, "--repair-catalog")
    assert repaired.returncode == 0, outcome(repaired)
    assert "testns/ghost" not in catalog_of(tree), f"--repair-catalog must drop it\n{outcome(repaired)}"
    assert package in catalog_of(tree), "the real package must survive the repair"


# ---------------------------------------------------------------------------
# S-011 / S-012 — the two per-package side objects
# ---------------------------------------------------------------------------


def seed_referrer(registry: str, repository: str, subject_digest: str, subject_bytes: bytes) -> str:
    """Attaches a cosign-shaped referrer to `subject_digest` and publishes the fallback referrers tag.

    `registry:2` accepts a manifest carrying `subject` but implements neither
    the referrers API nor the fallback tag it would otherwise synthesize
    (verified: `/v2/<name>/referrers/<digest>` answers 404 and the
    `<algo>-<hex>` tag stays absent). So the tag is written here — which is
    also the path C-024 cares about, since `pull_referrers` falls back to it
    and a hand-rolled endpoint call would read such a registry as "no
    referrers".
    """
    child = json.loads(fetch_manifest(registry, repository, subject_digest)[1])["manifests"][0]
    child_manifest = json.loads(fetch_manifest(registry, repository, child["digest"])[1])

    referrer = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": "application/vnd.dev.cosign.simplesigning+json",
        "config": child_manifest["config"],
        "layers": child_manifest["layers"],
        "subject": {
            "mediaType": INDEX_MEDIA_TYPE,
            "digest": subject_digest,
            "size": len(subject_bytes),
        },
    }
    referrer_bytes = json.dumps(referrer).encode()
    referrer_digest = put_manifest(
        registry, repository, "referrer", referrer_bytes, "application/vnd.oci.image.manifest.v1+json"
    )

    algorithm, _, hex_digest = subject_digest.partition(":")
    fallback = {
        "schemaVersion": 2,
        "mediaType": INDEX_MEDIA_TYPE,
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": referrer_digest,
                "size": len(referrer_bytes),
                "artifactType": referrer["artifactType"],
            }
        ],
    }
    put_manifest(registry, repository, f"{algorithm}-{hex_digest}", json.dumps(fallback).encode(), INDEX_MEDIA_TYPE)
    return referrer_digest


def test_a_package_carrying_a_referrer_fails_with_a_counted_error(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-011: v1 detects referrers and fails the package rather than copying them silently."""
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    referrer_digest = seed_referrer(registry, package, digest, body)

    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec, "--format", "json")
    assert result.returncode != 0, f"a detected referrer must fail the package\n{outcome(result)}"

    report = report_json(result)
    assert report["counters"]["failed"] == 1, report
    detail = report["sources"][0]["packages"][0].get("detail") or ""
    assert referrer_digest in detail, f"the error must name what was not copied\n{detail}"
    assert not (output / SOURCE_AS / "p" / f"{package}.json").exists(), (
        "a package whose referrers were not copied must not enter the index"
    )


def test_the_package_description_travels(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """S-012: `__ocx.desc` is copied per package; a package without one is a no-op, not a failure.

    The described package's root also carries the `desc` object a real
    published root carries (`digest`/`logo`/`readme`), which the mirror does
    not model — so this doubles as C-028's verbatim-passthrough check on a
    field only a production-shaped fixture has.
    """
    described = f"testns/{unique_mirror_repo}_described"
    bare = f"testns/{unique_mirror_repo}_bare"
    described_digest, described_body = seed_version(ocx_binary, registry, described, "1.0.0", tmp_path / "push-d", b"d")
    bare_digest, bare_body = seed_version(ocx_binary, registry, bare, "1.0.0", tmp_path / "push-b", b"b")
    push_ocx_description(ocx_binary, registry, described, tmp_path / "describe")
    source_description = fetch_manifest(registry, described, "__ocx.desc")[0]

    desc = {
        "digest": source_description,
        "logo": f"sha256:{'1' * 64}",
        "readme": f"sha256:{'2' * 64}",
    }
    write_published_index_tree(
        published_index_server.dir,
        [
            tree_package(
                registry,
                described,
                {"1.0.0": described_digest},
                {described_digest: described_body},
                desc=desc,
            ),
            tree_package(registry, bare, {"1.0.0": bare_digest}, {bare_digest: bare_body}),
        ],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec)
    assert result.returncode == 0, f"a package with no description must not fail\n{outcome(result)}"
    assert fetch_manifest(mirror_registry, destination_repository(described), "__ocx.desc")[0] == source_description
    assert manifest_absent(mirror_registry, destination_repository(bare), "__ocx.desc")

    mirrored_root = json.loads((output / SOURCE_AS / "p" / f"{described}.json").read_text())
    assert mirrored_root.get("desc") == desc, (
        "a package-level field the mirror does not model must ride through the rewrite verbatim"
    )


# ---------------------------------------------------------------------------
# The consumer side — what the produced tree is worth to a real `ocx`
# ---------------------------------------------------------------------------

# A consumer parses the first path segment as a registry only when it looks
# like a host, so the two scenarios below give the source a dotted `as:`. Every
# other scenario here never reads the tree back through `ocx`, so it keeps the
# plain one and exercises the "as: is one path component" rule instead.
CONSUMER_AS = "source.example"


@pytest.fixture()
def serve_directory():
    """Serves a directory over loopback HTTP — the operator's `git push` + static host, in-process."""
    servers: list[http.server.HTTPServer] = []

    def start(directory: Path) -> str:
        httpd = http.server.HTTPServer(
            ("127.0.0.1", 0),
            lambda *args: http.server.SimpleHTTPRequestHandler(*args, directory=str(directory)),
        )
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        servers.append(httpd)
        return f"http://127.0.0.1:{httpd.server_address[1]}"

    yield start
    for httpd in servers:
        httpd.shutdown()


def consumer_runner(ocx_binary: Path, ocx_home: Path, index_url: str, mirror_registry: str) -> OcxRunner:
    """A real `ocx` pointed at the served mirror tree, knowing no registry but the destination.

    This is the deployment the feature exists for, verbatim:
    `[registries."<ns>"] index = "https://pages.corp/<ns>"`. `trusted_hosts`
    is the *consumer's* SSRF floor, not the mirror's — a root whose
    `repository` names a host other than the identifier's own registry is a
    rewrite, and ocx refuses a rewrite onto loopback unless the namespace
    opts in.
    """
    runner = OcxRunner(ocx_binary, ocx_home, mirror_registry)
    (ocx_home / "config.toml").write_text(
        f'[registries."{CONSUMER_AS}"]\nindex = "{index_url}"\ntrusted_hosts = ["localhost", "127.0.0.1"]\n'
    )
    runner.env["OCX_INSECURE_REGISTRIES"] = f"{mirror_registry},{index_url.removeprefix('http://')}"
    return runner


def test_a_real_ocx_installs_from_the_mirrored_tree(
    sync: MirrorRunner,
    ocx_binary: Path,
    ocx_home: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    serve_directory,
    tmp_path: Path,
) -> None:
    """S-001, consumer half: the served tree installs with the upstream registry named nowhere.

    The consumer knows `mirror_registry` and the served tree, and nothing
    else — the only thing that can point it at content is the rewritten
    `repository` the run wrote. An unrewritten root would send it to
    `localhost:5001`, which its `OCX_INSECURE_REGISTRIES` does not name.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url(), as_name=CONSUMER_AS)],
    )
    assert run_sync(sync, spec).returncode == 0

    consumer = consumer_runner(ocx_binary, ocx_home, serve_directory(output / CONSUMER_AS), mirror_registry)
    logical = f"{CONSUMER_AS}/{package}:1.0.0"

    resolved = consumer.json("package", "inspect", logical)
    assert resolved["packages"][0]["pinned_digest"], resolved

    installed = consumer.run("package", "install", logical, check=False)
    assert installed.returncode == 0, f"a real ocx must install from the mirrored tree\n{installed.stderr}"


def test_a_scheme_less_repository_pointer_fails_a_real_consumer(
    sync: MirrorRunner,
    ocx_binary: Path,
    ocx_home: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    serve_directory,
    tmp_path: Path,
) -> None:
    """C-014, shown rather than argued: a `repository` without `oci://` makes the tree unresolvable.

    The catalog entry is re-derived from the mutated bytes so the tree stays
    self-consistent — otherwise the consumer would be refusing the digest
    mismatch, not the pointer, and the demonstration would prove nothing.

    Asserted as "resolved before, does not resolve after" rather than on a
    specific exit code: the ADR says a scheme-less pointer is exit 65 at every
    consumer, and the measured code depends on how the resolve degrades (75
    when the malformed root is read as a local-index failure and the walk
    falls through to a source). The mirror-side property — such a tree is
    unusable, so C-014 must round-trip the pointer before writing it — is the
    same under either.
    """
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    output = tmp_path / "public"
    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=output,
        sources=[source_spec(registry, published_index_server.url(), as_name=CONSUMER_AS)],
    )
    assert run_sync(sync, spec).returncode == 0

    index_url = serve_directory(output / CONSUMER_AS)
    logical = f"{CONSUMER_AS}/{package}:1.0.0"

    intact = consumer_runner(ocx_binary, ocx_home, index_url, mirror_registry)
    assert intact.run("package", "install", logical, check=False).returncode == 0, (
        "the produced tree must install before it is broken, or the refusal below proves nothing"
    )

    tree = output / CONSUMER_AS
    root_path = tree / "p" / f"{package}.json"
    root = json.loads(root_path.read_text())
    root["repository"] = root["repository"].removeprefix("oci://")
    root_bytes = json.dumps(root).encode()
    root_path.write_bytes(root_bytes)
    catalog_path = tree / "c" / "index.json"
    catalog = json.loads(catalog_path.read_text())
    catalog["packages"][package] = f"sha256:{hashlib.sha256(root_bytes).hexdigest()}"
    catalog_path.write_text(json.dumps(catalog))

    # A second, empty `OCX_HOME`: the first install committed the tag pin and
    # the dispatch object into its own local index, so re-asking there would
    # answer from that snapshot and never look at the mutated pointer.
    cold_home = tmp_path / "consumer-cold"
    cold_home.mkdir()
    cold = consumer_runner(ocx_binary, cold_home, index_url, mirror_registry)

    broken = cold.run("package", "install", logical, check=False)
    assert broken.returncode != 0, (
        f"a scheme-less physical pointer must make the package unusable\n{broken.stdout}\n{broken.stderr}"
    )


# ---------------------------------------------------------------------------
# The report contract (C-042, C-026 item c)
# ---------------------------------------------------------------------------


def test_the_json_report_carries_the_four_counters_and_per_package_rows(
    sync: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    mirror_registry: str,
    unique_mirror_repo: str,
    published_index_server,
    tmp_path: Path,
) -> None:
    """C-042: `--format json` parses and carries `{total, copied, skipped, failed}` plus package rows."""
    package = f"testns/{unique_mirror_repo}"
    digest, body = seed_version(ocx_binary, registry, package, "1.0.0", tmp_path / "push")
    write_published_index_tree(
        published_index_server.dir,
        [tree_package(registry, package, {"1.0.0": digest}, {digest: body})],
    )

    spec = tmp_path / "registry.yml"
    write_registry_spec(
        spec,
        target_registry=mirror_registry,
        target_repository=TARGET_PREFIX,
        output=tmp_path / "public",
        sources=[source_spec(registry, published_index_server.url())],
    )

    result = run_sync(sync, spec, "--format", "json")
    assert result.returncode == 0, outcome(result)

    report = report_json(result)
    assert set(report["counters"]) == {"total", "copied", "skipped", "failed"}, report
    assert report["counters"] == {"total": 1, "copied": 1, "skipped": 0, "failed": 0}, report
    assert [source["as_name"] for source in report["sources"]] == [SOURCE_AS], report
    assert [(row["name"], row["outcome"]) for row in report["sources"][0]["packages"]] == [(package, "copied")], report
