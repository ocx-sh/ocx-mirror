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
  scenario opts a host in);
- the mirror-signing harness surface (WP 5, C-073): zot's native Referrers
  API against `registry:2`'s absence of one, GC-disabled retention of an
  untagged manifest, the `sigstore_stack` skip guard (S-059), sibling-clone
  resolution from the main checkout, and `MirrorRunner.env`'s signing
  whitelist.

Each shown both green and red.
"""

from __future__ import annotations

import hashlib
import json
import os
import stat
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from src.helpers import (
    DEFAULT_SIGSTORE_COMPOSE,
    PROJECT_ROOT,
    SIGSTORE_SERVICES,
    SigstoreStack,
    fetch_manifest,
    main_checkout_root,
    mint_identity_token,
    push_stub_ocx_package,
    put_manifest,
    registry_is_reachable,
    sigstore_skip_reason,
    wait_for_sigstore,
)
from src.mirror_runner import MirrorRunner
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


# ---------------------------------------------------------------------------
# Mirror-signing harness (WP 5, C-073) — zot, sigstore bring-up, env whitelist
# ---------------------------------------------------------------------------

_OCI_INDEX_MEDIA_TYPE = "application/vnd.oci.image.index.v1+json"

#: The signing variables `MirrorRunner.env` is contracted to whitelist
#: (C-073). Held here as the *expected* list rather than imported from
#: `mirror_runner` so the assertions below cannot be satisfied by whatever
#: the implementation happens to loop over.
SIGNING_ENV_VARS = (
    "SIGSTORE_FULCIO_URL",
    "SIGSTORE_REKOR_URL",
    "MIRROR_SIGNING_KEY",
    "MIRROR_KEY_PASSPHRASE",
    "OCX_CONFIG",
)


def _referrers_response(registry: str, repository: str, digest: str) -> tuple[int, str, bytes]:
    """GET `/v2/<name>/referrers/<digest>`, returning (status, content-type, body).

    A 404 is an *answer* here, not a transport failure, so `HTTPError` is
    caught rather than propagated: C-063 keys the destination-capability
    probe on the status code (404 and 405 both mean Unsupported), and the
    body a registry sends with it is not part of the contract.
    """
    request = urllib.request.Request(f"http://{registry}/v2/{repository}/referrers/{digest}")
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status, response.headers.get("Content-Type", ""), response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers.get("Content-Type", ""), error.read()


def test_zot_answers_the_referrers_api(
    ocx_binary: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """C-073/S-058: zot is the native-Referrers-API leg of the harness, so
    `GET /v2/<name>/referrers/<digest>` must answer 200 with an OCI index --
    that is the capability the copy path's Supported route depends on."""
    repository = f"testns/{unique_mirror_repo}"
    push_stub_ocx_package(ocx_binary, zot_registry, f"{repository}:1.0.0", tmp_path / "push-setup")
    digest, _ = fetch_manifest(zot_registry, repository, "1.0.0")

    status, content_type, body = _referrers_response(zot_registry, repository, digest)

    assert status == 200, f"zot must implement the Referrers API; answered {status} with {body[:120]!r}"
    assert content_type.startswith(_OCI_INDEX_MEDIA_TYPE), (
        f"the referrers response must be typed as an OCI index; got {content_type!r}"
    )
    document = json.loads(body)
    assert document["mediaType"] == _OCI_INDEX_MEDIA_TYPE
    assert isinstance(document["manifests"], list), "an OCI index carries a manifests array"


def test_registry_two_does_not_answer_the_referrers_api(
    ocx_binary: Path,
    registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """C-063/C-073: the `registry` leg is distribution v2, which implements no
    Referrers API -- the fallback-index route exists for exactly this.

    The subject is pushed first and read back, so the refusal below cannot be
    a missing repository. The assertion is on the status alone: distribution
    answers this route with Go's own plain-text `404 page not found` and no
    `{"errors": [...]}` envelope, so a probe that tried to parse a
    distribution error body would read Unsupported off a JSONDecodeError."""
    repository = f"testns/{unique_mirror_repo}"
    push_stub_ocx_package(ocx_binary, registry, f"{repository}:1.0.0", tmp_path / "push-setup")
    digest, _ = fetch_manifest(registry, repository, "1.0.0")

    status, content_type, _ = _referrers_response(registry, repository, digest)

    assert status in (404, 405), (
        f"an unsupported Referrers API must surface as 404 or 405 (C-063); got {status}"
    )
    assert not content_type.startswith(_OCI_INDEX_MEDIA_TYPE), (
        "a registry answering with an OCI index here supports the Referrers API -- "
        "the fallback-index leg of the harness would then be untested"
    )


def test_untagged_manifest_survives_on_zot(
    ocx_binary: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """C-073: zot garbage-collects untagged manifests, and a referrer is untagged,
    so the harness ships `test/zot-config.json` with GC disabled.

    Two halves, because there is no bounded behavioural seam for the whole
    clause: zot exposes no GC trigger and its `gcDelay` default is an hour,
    so no wait a test suite can afford distinguishes "GC is off" from "GC has
    not run yet". The *control* is therefore the mounted config asserting
    `storage.gc` is false; the behavioural half is that an unreferenced,
    untagged manifest lands and reads back byte-for-byte after an explicit
    settle, which is what a referrer copy will depend on."""
    repository = f"testns/{unique_mirror_repo}"
    push_stub_ocx_package(ocx_binary, zot_registry, f"{repository}:1.0.0", tmp_path / "push-setup")
    _, tagged_body = fetch_manifest(zot_registry, repository, "1.0.0")
    child = json.loads(tagged_body)["manifests"][0]

    # An index no tag and no other manifest points at -- the reachability
    # shape of a referrer, which is what a GC pass would sweep.
    orphan = {
        "schemaVersion": 2,
        "mediaType": _OCI_INDEX_MEDIA_TYPE,
        "manifests": [{"mediaType": child["mediaType"], "digest": child["digest"], "size": child["size"]}],
    }
    orphan_body = json.dumps(orphan, separators=(",", ":")).encode()
    orphan_digest = f"sha256:{hashlib.sha256(orphan_body).hexdigest()}"
    pushed_digest = put_manifest(zot_registry, repository, orphan_digest, orphan_body, _OCI_INDEX_MEDIA_TYPE)
    assert pushed_digest == orphan_digest, "a manifest PUT by digest must land under that exact digest"

    time.sleep(2.0)

    read_digest, read_body = fetch_manifest(zot_registry, repository, orphan_digest)
    assert read_digest == orphan_digest
    assert read_body == orphan_body, "the untagged manifest must survive verbatim"

    config = json.loads((PROJECT_ROOT / "test" / "zot-config.json").read_text())
    assert config["storage"]["gc"] is False, (
        "zot must run with GC disabled or referrer-copy assertions go flaky in a way "
        "that reads as a copy bug (C-073)"
    )


def test_sigstore_stack_skips_with_a_reason_naming_the_missing_compose_file(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """C-073/S-059: an `OCX_SIGSTORE_COMPOSE` pointing at a missing file skips with a
    reason visible in the log -- never fails, and never passes silently.

    The reason must name the path it looked at: "keyless tier skipped" with no
    path is indistinguishable from a broken default on a machine that does
    have the sibling clone.

    Asserted on `sigstore_skip_reason()`, which is the string the fixture
    skips with, rather than by requesting the fixture under a monkeypatched
    environment: pytest caches a session-scoped fixture's `Skipped` and
    re-raises it for every later consumer, so provoking one here would skip
    the live-stack tests for the rest of the session."""
    missing = tmp_path / "no-such-ocx" / "test" / "docker-compose.yml"
    monkeypatch.setenv("OCX_SIGSTORE_COMPOSE", str(missing))

    reason = sigstore_skip_reason()

    assert reason is not None, "a missing compose file must produce a skip reason"
    assert str(missing) in reason, f"the skip reason must name the path it looked at; got {reason!r}"
    assert "OCX_SIGSTORE_COMPOSE" in reason, "the skip reason must name the override variable"


def test_main_checkout_root_resolves_past_a_worktree() -> None:
    """C-073: the sibling clone resolves from the *main checkout*, never from
    `__file__` -- inside an agent worktree the latter points one level too shallow.

    A main checkout carries `.git` as a directory; a linked worktree carries
    it as a *file* holding a `gitdir:` pointer. That distinction is the
    independent property here, so the assertion does not restate the
    implementation's own `git rev-parse` call."""
    root = main_checkout_root()

    assert (root / ".git").is_dir(), (
        f"main_checkout_root() must land on a main checkout, but {root}/.git is not a directory"
    )
    if PROJECT_ROOT == root:
        # Plain checkout: the two agree, and the worktree branch below is
        # unreachable rather than silently skipped.
        assert (PROJECT_ROOT / ".git").is_dir()
    else:
        assert (PROJECT_ROOT / ".git").is_file(), (
            "the two roots may only differ when this really is a linked worktree"
        )


def test_default_sigstore_compose_is_the_main_checkout_sibling() -> None:
    """C-073: `DEFAULT_SIGSTORE_COMPOSE` is `<main checkout>/../ocx/test/docker-compose.yml`.

    The second assertion is the one with teeth: inside a worktree the
    pre-fix, `PROJECT_ROOT`-derived default resolved to
    `.agents/worktrees/ocx/...`, which exists nowhere, and the keyless tier
    skipped itself away on a machine that had the clone all along."""
    root = main_checkout_root()

    assert DEFAULT_SIGSTORE_COMPOSE == root.parent / "ocx" / "test" / "docker-compose.yml"
    if PROJECT_ROOT != root:
        assert not str(DEFAULT_SIGSTORE_COMPOSE).startswith(f"{PROJECT_ROOT.parent}{os.sep}"), (
            "the default must not resolve as a sibling of the *worktree*"
        )


def test_sigstore_services_names_the_log_signer() -> None:
    """C-073/D6a: the service list is one named constant, and `trillian-log-signer`
    must be in it -- it appears in no `depends_on`, so without it Rekor accepts
    entries that are never integrated and every keyless test fails in a way that
    reads as a Fulcio bug."""
    assert "trillian-log-signer" in SIGSTORE_SERVICES
    assert {"dex", "fulcio", "rekor"} <= set(SIGSTORE_SERVICES), (
        "the three services wait_for_sigstore polls must be among those brought up"
    )


# ---------------------------------------------------------------------------
# MirrorRunner.env whitelist (WP 5, C-073)
# ---------------------------------------------------------------------------


def _runner(tmp_path: Path, registry: str = "localhost:5001") -> MirrorRunner:
    """A `MirrorRunner` built purely for its `.env`. The binary path is never
    dereferenced by the constructor, so no build is needed to assert on the
    environment it would hand a child."""
    return MirrorRunner(tmp_path / "ocx-mirror", registry, tmp_path / "work")


def test_mirror_runner_omits_an_unset_signing_variable(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """C-073/S-061: a whitelisted variable that is unset in the parent stays *absent*
    for the child, never forwarded as an empty string -- an empty value would shadow
    the absence the mirror's own `SignMaterialMissing` path is supposed to see."""
    for name in SIGNING_ENV_VARS:
        monkeypatch.delenv(name, raising=False)

    env = _runner(tmp_path).env

    for name in SIGNING_ENV_VARS:
        assert name not in env, f"{name} is unset in the parent and must not appear in the child env"


def test_mirror_runner_forwards_a_set_signing_variable_verbatim(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """C-073: `MirrorRunner.env` is a constructed whitelist, so every variable a
    signing fixture names under `env://` reaches the child only by being listed --
    and it reaches it unchanged."""
    for name in SIGNING_ENV_VARS:
        monkeypatch.setenv(name, f"sentinel-for-{name}")

    env = _runner(tmp_path).env

    for name in SIGNING_ENV_VARS:
        assert env[name] == f"sentinel-for-{name}", f"{name} must be forwarded verbatim"


def test_mirror_runner_marks_both_registries_insecure(tmp_path: Path) -> None:
    """C-073: `OCX_INSECURE_REGISTRIES` is comma-joined to include zot as well as the
    run's own registry, so a push into the native-Referrers-API leg needs no
    per-invocation flag."""
    env = _runner(tmp_path, "localhost:5001").env

    addresses = env["OCX_INSECURE_REGISTRIES"].split(",")

    assert "localhost:5001" in addresses
    assert os.environ.get("ZOT_REGISTRY", "localhost:5011") in addresses


def test_mirror_runner_follows_a_zot_registry_override(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """C-073 (amended 2026-09-02): the runner and the `zot_registry` fixture read
    `ZOT_REGISTRY` from one place. Two independent reads mean a machine that moves
    zot off 5011 gets a runner still declaring 5011 insecure, and the push fails on
    TLS against an address nothing serves."""
    monkeypatch.setenv("ZOT_REGISTRY", "localhost:5999")

    addresses = _runner(tmp_path).env["OCX_INSECURE_REGISTRIES"].split(",")

    assert "localhost:5999" in addresses, (
        "the runner must resolve the zot address from ZOT_REGISTRY, not hardcode it"
    )


# ---------------------------------------------------------------------------
# Signing fixture rendering and the Sigstore bring-up (WP 5, C-073)
# ---------------------------------------------------------------------------


def test_render_signing_fixture_substitutes_both_placeholders(tmp_path: Path) -> None:
    """C-073 (amended 2026-09-02): `render_signing_fixture` is the one seat that
    substitutes `__TRUSTED_ROOT_PATH__` and `__IDENTITY_TOKEN_PATH__`.

    Both fixtures ship a literal placeholder because neither path survives
    being baked in: the trusted root sits in a sibling checkout resolved at
    runtime, and the dex token is minted per session. A rendered spec still
    carrying either placeholder is a spec that names a file called
    `__IDENTITY_TOKEN_PATH__`, which fails as a missing-file error a long way
    from the cause.

    The import is local so a missing symbol fails this one test rather than
    erroring collection for the whole module."""
    from src.helpers import render_signing_fixture

    trusted_root = tmp_path / "trusted_root.json"
    trusted_root.write_text("{}\n")
    token_path = tmp_path / "identity-token"
    token_path.write_text("header.payload.signature\n")
    dest = tmp_path / "signing-spec"

    returned = render_signing_fixture(dest, trusted_root, token_path)

    assert returned.exists(), "the returned path must name a file that was written"
    assert dest in returned.parents, f"the returned path must live under dest; got {returned}"

    rendered_spec = (dest / "mirror.yml").read_text()
    rendered_config = (dest / "config.toml").read_text()

    assert "__IDENTITY_TOKEN_PATH__" not in rendered_spec, "no placeholder may survive rendering"
    assert "__TRUSTED_ROOT_PATH__" not in rendered_config, "no placeholder may survive rendering"
    assert f"file://{token_path}" in rendered_spec, "the identity token must be named as a file:// ref"
    assert str(trusted_root) in rendered_config

    # C-073: the decoy is what makes S-061's "the publish side ignores this
    # config's Fulcio" assertion falsifiable -- a publish that fell through to
    # the config would fail loudly against an unresolvable host. It must
    # survive rendering, and no rekor override may join it.
    assert 'fulcio_url = "https://fulcio.invalid"' in rendered_config
    assert "rekor_url" not in rendered_config, "the verification config carries no rekor override"


def test_wait_for_sigstore_leaves_every_polled_endpoint_answering(sigstore_stack: SigstoreStack) -> None:
    """C-073/D6: readiness is polled from the host -- dex `/dex/healthz`, Fulcio
    `/api/v2/trustBundle`, Rekor `/api/v1/log` -- never a compose `healthcheck:`,
    because four of the seven images are distroless and a healthcheck on them is a
    green that never ran.

    Ports come from `OCX_TEST_DEX_PORT`/`OCX_TEST_FULCIO_PORT`/`OCX_TEST_REKOR_PORT`
    so a machine running a second stack off the defaults still resolves."""
    wait_for_sigstore(sigstore_stack.compose_file)

    endpoints = (
        ("dex", os.environ.get("OCX_TEST_DEX_PORT", "5556"), "/dex/healthz"),
        ("fulcio", os.environ.get("OCX_TEST_FULCIO_PORT", "5555"), "/api/v2/trustBundle"),
        ("rekor", os.environ.get("OCX_TEST_REKOR_PORT", "3000"), "/api/v1/log"),
    )
    for service, port, path in endpoints:
        with urllib.request.urlopen(f"http://localhost:{port}{path}", timeout=10) as response:
            assert response.status == 200, f"{service} answered {response.status} after wait_for_sigstore"


def test_mint_identity_token_writes_a_private_jwt(sigstore_stack: SigstoreStack, tmp_path: Path) -> None:
    """C-073/D1: `ocx package sign` has no `--identity-token <VALUE>` flag -- a token
    in argv is world-readable in /proc -- so the token reaches it as a file, and
    `sign` refuses a group- or world-readable one."""
    target = tmp_path / "identity-token"

    written = mint_identity_token(target)

    assert written == target
    assert target.read_text().strip().count(".") == 2, "a dex identity token is a three-segment JWT"
    assert stat.S_IMODE(target.stat().st_mode) == 0o600, "the token file must be readable only by its owner"
