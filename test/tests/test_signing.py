# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Acceptance for signing what the mirror publishes (WP 2).

Contract source: ``C-052``, ``C-054``–``C-059`` and scenarios ``S-050``,
``S-051``, ``S-052``, ``S-061``, ``S-062`` in
``.claude/state/plans/plan_mirror_signing.md``.

Every test here drives the **whole** pipeline — plan, prepare, a synthesised
green JUnit, push — against a real registry and the sibling ocx checkout's
Sigstore stack, then reads the registry back with ``ocx package verify``. The
registry is the witness: a run summary saying ``published`` proves nothing
about whether a signature was attached, and the D2 division of labour (each
platform manifest signed by its push, the index by the closing sweep) is only
distinguishable by asking the registry about both.

The ``ocx`` under test is ``real_ocx_binary`` — built from the ``external/ocx``
submodule — never ``test/bin/ocx``: ``push --sign --fulcio-url --rekor-url``
is OCX-C-5, which no released ``ocx`` carries yet.

**Why the registry is asked directly.** ``ocx package verify --platform`` was
tried first and is *not* falsifiable here: with the index signed and the
platform manifest bare it still exits 0, so it cannot tell the two halves of D2
apart. Every signing assertion below therefore reads the Referrers API for the
subject digest in question, and uses ``verify`` only for the separate claim
that what was written checks out against the stack that issued it.

**Not covered here, deliberately.** ``S-062``'s "and no Rekor entry" half is
pinned by the ``--no-rekor-upload`` argv unit test in
``src/pipeline/ocx_cli/sign/tests.rs``, which is where it is falsifiable: the
flag either is or is not in the child's argv, and reading a bundle back to
find the absence of a ``tlogEntries`` array tests the bundle writer rather
than this pipeline. ``S-052``'s plugin-dispatch shape lives in
``test_signing_dispatch.py``, which drives the real ``ocx mirror package
pipeline push`` invocation: the scrub *can* reach these decisions — naming
``OCX_IDENTITY_TOKEN`` in ``sign.keyless.identity_token`` empties the mirror's
own lookup — so the mirror refuses those three variable names at spec
validation rather than letting the run start.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from src.helpers import render_signing_fixture, sigstore_trusted_root
from src.mirror_runner import MirrorRunner

#: The SAN and issuer the sibling checkout's dex mints, and so what
#: ``ocx package verify`` must be told to require. Duplicated from ocx's own
#: ``test/src/helpers.py`` rather than imported: that package is not on this
#: harness's path, and these two strings are part of the *stack's* contract,
#: which C-073 already pins by naming the compose file.
SIGSTORE_IDENTITY = "ocx-test@example.com"
SIGSTORE_ISSUER = "http://dex:5556/dex"

#: The committed cosign key pair the sibling checkout signs its own key-mode
#: fixtures with, and its password. Not a secret — see the README beside it.
_OCX_KEYS = Path(__file__).resolve().parents[2] / "external" / "ocx" / "test" / "tests" / "fixtures" / "golden" / "keys"
COSIGN_KEY = _OCX_KEYS / "cosign.key"
COSIGN_PUB = _OCX_KEYS / "cosign.pub"
KEY_PASSWORD = "ocxtest"

#: Sigstore bundle v0.3 — the artifact type every signature referrer this
#: pipeline writes carries, mirroring ocx's own
#: `oci::referrer::media_types::SIGSTORE_BUNDLE_V03`.
SIGSTORE_BUNDLE_V03 = "application/vnd.dev.sigstore.bundle.v0.3+json"

#: `platforms."linux/amd64".containers[0].image` from the signing fixture,
#: slugged the way `push` slugs it when it looks the JUnit file up.
CONTAINER_ID = "ubuntu_24_04"
PLATFORM = "linux/amd64"
PLATFORM_SLUG = "linux_amd64"


def _passing_junit(version: str) -> str:
    """JUnit XML for one green (version, platform, container) triple.

    Stands in for the GHA `test` matrix leg exactly as `test_mirror_e2e.py`
    does — `push` reads nothing else from it, so producing the XML directly
    exercises the same code path without pullable base images.
    """
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{PLATFORM_SLUG}.{CONTAINER_ID}"
             tests="1" failures="0" errors="0" skipped="0"
             timestamp="2026-05-13T10:00:00Z" time="1.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="{PLATFORM}"/>
      <property name="ocx.image" value="ubuntu:24.04"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{PLATFORM_SLUG}.{CONTAINER_ID}" time="1.0"/>
  </testsuite>
</testsuites>"""


_INDEX_ACCEPT = ", ".join((
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
))


def _manifest(registry: str, repository: str, reference: str) -> tuple[str, dict]:
    """The digest and body the registry serves for ``reference``.

    The digest comes from ``Docker-Content-Digest`` rather than being hashed
    here: it is the identity every referrer is filed under, and recomputing it
    locally would be this test agreeing with itself about canonical bytes.
    """
    request = urllib.request.Request(
        f"http://{registry}/v2/{repository}/manifests/{reference}",
        headers={"Accept": _INDEX_ACCEPT},
    )
    with urllib.request.urlopen(request) as resp:
        return resp.headers["Docker-Content-Digest"], json.load(resp)


def _signature_referrers(registry: str, repository: str, digest: str) -> list[dict]:
    """Sigstore bundles filed against ``digest`` through the Referrers API.

    Asked of the registry directly rather than inferred from ``ocx package
    verify``: a `--platform`-narrowed verify passes on an index whose *index*
    is signed even when the platform manifest underneath carries nothing, so
    it cannot tell the two halves of D2 apart — which is the whole thing these
    tests exist to distinguish. Zot answers the OCI 1.1 route natively, so this
    needs no fallback-tag path.
    """
    with urllib.request.urlopen(f"http://{registry}/v2/{repository}/referrers/{digest}") as resp:
        index = json.load(resp)
    return [m for m in index.get("manifests", []) if m.get("artifactType") == SIGSTORE_BUNDLE_V03]


def _registry_tags(registry: str, repository: str) -> set[str]:
    """Tags the registry actually carries for ``repository``."""
    try:
        with urllib.request.urlopen(f"http://{registry}/v2/{repository}/tags/list") as resp:
            return set(json.load(resp)["tags"] or [])
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return set()
        raise


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def signing_spec(
    tmp_path: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    asset_server,
    sigstore_stack,
) -> Path:
    """The signing fixture rendered per test, with every placeholder resolved.

    Rendered here rather than reused from the session-scoped ``sigstore_stack``
    because the target repository has to be unique per test: two tests sharing
    one repository would have the second report ``skipped_existing`` and
    publish — and therefore sign — nothing at all, which reads as a pass.

    Zot rather than the ``registry`` fixture: it implements the OCI 1.1
    Referrers API, so a signature lands as a referrer of its subject and
    ``verify`` finds it there rather than behind the fallback tag. It is also
    this harness's own container, where 5001 is shared with the sibling
    checkout's compose project.
    """
    spec_dir = tmp_path / "spec"
    render_signing_fixture(
        spec_dir,
        sigstore_trusted_root(sigstore_stack.compose_file),
        sigstore_stack.token_path,
    )

    (asset_server.dir / "shfmt_v3.7.0_linux_amd64").write_text("#!/bin/sh\necho v3.7.0\n")

    spec_path = spec_dir / "mirror.yml"
    spec_path.write_text(
        spec_path.read_text()
        .replace("__ASSET_PORT__", str(asset_server.port))
        .replace("localhost:5000", zot_registry)
        .replace("test-shfmt-signing", unique_mirror_repo)
    )
    return spec_path


@pytest.fixture()
def signing_mirror(
    mirror_binary: Path,
    real_ocx_binary: Path,
    zot_registry: str,
    tmp_path: Path,
    sigstore_stack,
) -> MirrorRunner:
    """A mirror runner wired to the local stack and the submodule's ``ocx``.

    ``OCX_BINARY_PIN`` rather than ``PATH``: whatever ``ocx`` this machine has
    installed predates OCX-C-5, so a run that resolved it would fail on an
    unknown ``--fulcio-url`` and the failure would read as a mirror bug.
    """
    runner = MirrorRunner(mirror_binary, zot_registry, tmp_path / "mirror-work")
    (tmp_path / "mirror-work").mkdir(parents=True, exist_ok=True)
    runner.env["OCX_BINARY_PIN"] = str(real_ocx_binary)
    runner.env["OCX_HOME"] = str(tmp_path / "ocx-home")
    runner.env["OCX_NO_PROJECT"] = "1"
    # The two endpoint variables `fixtures/signing/mirror.yml` names under
    # `env://`. The mirror resolves them itself and emits them as flags, which
    # is exactly what the config decoy below is there to prove.
    runner.env["SIGSTORE_FULCIO_URL"] = sigstore_stack.fulcio_url
    runner.env["SIGSTORE_REKOR_URL"] = sigstore_stack.rekor_url
    return runner


def _publish(mirror: MirrorRunner, spec: Path, work: Path, *, check: bool = True):
    """Drive plan → prepare → flatten → push, returning ``(version, result)``.

    The flattening step reproduces what the generated ``prepare`` job does to
    the work directory, including its ``+`` → ``_`` slug, so ``push`` is
    invoked over exactly the artifact layout a real run hands it.
    """
    work.mkdir(parents=True, exist_ok=True)
    junit_dir = work / "junit"
    junit_dir.mkdir(exist_ok=True)
    bundles_dir = work / "bundles"
    bundles_dir.mkdir(exist_ok=True)

    plan_path = work / "plan.json"
    plan_path.write_text(
        mirror.run("package", "pipeline", "plan", "--spec", str(spec), "--format", "json").stdout
    )
    version = json.loads(plan_path.read_text())["versions"][0]["version"]

    mirror.run(
        "package", "pipeline", "prepare",
        "--spec", str(spec),
        "--version", version,
        "--work-dir", str(work),
        "--plan", str(plan_path),
    )

    prepared = work / version.replace("+", "_") / PLATFORM_SLUG
    shutil.copy(prepared / "bundle.tar.xz", bundles_dir / f"bundle-{version}-{PLATFORM_SLUG}.tar.xz")
    shutil.copy(
        prepared / "metadata.json",
        bundles_dir / f"bundle-{version}-{PLATFORM_SLUG}-metadata.json",
    )
    (junit_dir / f"junit-{version}-{PLATFORM_SLUG}-{CONTAINER_ID}.xml").write_text(_passing_junit(version))

    result = mirror.run(
        "package", "pipeline", "push",
        "--spec", str(spec),
        "--junit-dir", str(junit_dir),
        "--bundles-dir", str(bundles_dir),
        "--write-summary", str(work / "run-summary.json"),
        check=check,
    )
    return version, result


def _verify(
    real_ocx_binary: Path,
    mirror: MirrorRunner,
    sigstore_stack,
    reference: str,
    *extra: str,
) -> subprocess.CompletedProcess[str]:
    """Run ``ocx package verify`` against the local stack, without ``check``.

    ``OCX_CONFIG`` names the fixture's own ``config.toml``, which carries the
    trusted root **and** the ``https://fulcio.invalid`` decoy: verify never
    dials Fulcio, so the decoy is inert here and is what makes the publish-side
    assertion in ``S-061`` non-vacuous.
    """
    env = dict(mirror.env)
    env["OCX_CONFIG"] = str(sigstore_stack.config_path)
    env["OCX_HOME"] = mirror.env["OCX_HOME"]
    return subprocess.run(
        [str(real_ocx_binary), "--format", "json", "package", "verify", reference, *extra],
        capture_output=True,
        text=True,
        env=env,
    )


# ---------------------------------------------------------------------------
# S-050 / S-061 — keyless against the named instance
# ---------------------------------------------------------------------------


def test_keyless_signs_every_platform_manifest_and_the_index(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    real_ocx_binary: Path,
    sigstore_stack,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """S-050: a `sign: { keyless: ... }` mirror publishes signed, both levels.

    Both halves are asserted because D2 splits them across two mechanisms and
    either can be missing while the other works: `push --sign` writes the
    platform manifest's signature inline, and only the closing
    `sign --tags-file` sweep reaches the index, whose digest was still moving
    while the platforms were landing.

    S-061's second clause rides along here rather than in a test of its own: a
    publish that fell through to `[trust.sigstore]` would dial
    `https://fulcio.invalid` — the decoy `config.toml` carries — and fail with
    a DNS error, so this run's green *is* the assertion that the endpoints came
    from the spec's own `env://` refs.
    """
    version, _ = _publish(signing_mirror, signing_spec, tmp_path / "work")

    summary = json.loads((tmp_path / "work" / "run-summary.json").read_text())
    assert summary["any_red"] is False, summary
    assert summary["versions"][0]["platforms_pushed"] == [PLATFORM], summary

    reference = f"{zot_registry}/{unique_mirror_repo}:{version}"
    common = [
        "--rekor-url", sigstore_stack.rekor_url,
        "--certificate-identity", SIGSTORE_IDENTITY,
        "--certificate-oidc-issuer", SIGSTORE_ISSUER,
    ]

    index_digest, index_body = _manifest(zot_registry, unique_mirror_repo, version)
    child_digest = index_body["manifests"][0]["digest"]

    # The sweep's half. `push --sign` never touches the index — its digest is
    # rewritten every time another platform merges in.
    # 1, not 5: the sweep signs once per distinct index per run. It used to
    # sign once per cascade tag, and all five tags of a version resolve to this
    # one digest, so a run filed five referrers against one subject.
    # `test_a_second_push_adds_one_signature_not_one_per_tag` is the guard on
    # the per-run half; this is the guard on the per-tag half.
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest)) == 1, (
        "the index carries no signature — the closing `sign --tags-file` sweep did not run"
    )
    # `push --sign`'s half. Asserted separately because the two are written by
    # two different mechanisms and either can be missing while the other works:
    # dropping the `--sign` tail from every push argv leaves this list empty
    # and every index-level assertion above still green.
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, child_digest)) == 1, (
        f"the {PLATFORM} manifest carries no signature — `push --sign` did not reach it"
    )

    # And what was written actually verifies against the stack that issued it.
    verified = _verify(real_ocx_binary, signing_mirror, sigstore_stack, reference, *common)
    assert verified.returncode == 0, f"the signature does not verify:\n{verified.stdout}\n{verified.stderr}"


# ---------------------------------------------------------------------------
# C-055 / S-051 — no reachable identity fails the package
# ---------------------------------------------------------------------------


def test_a_sign_block_with_no_reachable_identity_fails_before_publishing(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """C-055/S-051: a mirror that cannot sign fails; it never publishes quietly.

    This is the scenario the whole feature exists to prevent. A cron box with
    no OIDC and no key, or a runner whose `SIGSTORE_FULCIO_URL` was never set,
    must red the job — publishing unsigned and reporting success is worse than
    publishing nothing, because nothing downstream can tell the difference.

    Exit code and stream are asserted separately (C-055): a message on stdout
    would be invisible to the workflow's log and a code of 1 would be
    indistinguishable from any other failure. 78 is `ConfigError` — the remedy
    is the runner's configuration, not the spec's syntax.

    And the registry is asked, not the summary: "no tag advanced" is a claim
    about the registry, and a run that pushed and then failed to sign would
    satisfy every summary-level assertion.
    """
    del signing_mirror.env["SIGSTORE_FULCIO_URL"]

    _, result = _publish(signing_mirror, signing_spec, tmp_path / "work", check=False)

    assert result.returncode == 78, (
        f"an unreachable signing endpoint must fail the package, got {result.returncode}"
        f"\nstdout: {result.stdout}\nstderr: {result.stderr}"
    )
    assert "sign.keyless.fulcio" in result.stderr, result.stderr
    assert "SIGSTORE_FULCIO_URL" in result.stderr, result.stderr
    assert result.stderr.count("SIGSTORE_FULCIO_URL") >= 1 and "SIGSTORE_FULCIO_URL" not in result.stdout, (
        "the failure must reach stderr, where the workflow log reads it"
    )
    assert _registry_tags(zot_registry, unique_mirror_repo) == set(), (
        "a package that could not sign must not have advanced a tag"
    )


# ---------------------------------------------------------------------------
# S-062 — key mode
# ---------------------------------------------------------------------------


def test_key_mode_signs_with_the_named_key(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    real_ocx_binary: Path,
    sigstore_stack,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """S-062: `sign: { key: { ref, passphrase } }` signs with that key pair.

    The passphrase reaches `ocx` through `OCX_KEY_PASSWORD` and never through
    argv — there is no flag for it, by ocx's own design — so a run that
    resolved it any other way could not decrypt the PEM and could not sign at
    all. The green is the evidence.

    Verified with the public half alone: `--key` conflicts with the certificate
    matchers, so a key-mode signature that had silently fallen through to
    keyless would fail here rather than pass under a looser check.
    """
    spec_text = signing_spec.read_text()
    head, _, _ = spec_text.partition("sign:")
    signing_spec.write_text(
        head
        + "sign:\n"
        + "  key:\n"
        + f"    ref: file://{COSIGN_KEY}\n"
        + "    passphrase: env://MIRROR_KEY_PASSPHRASE\n"
    )
    signing_mirror.env["MIRROR_KEY_PASSPHRASE"] = KEY_PASSWORD

    version, _ = _publish(signing_mirror, signing_spec, tmp_path / "work")

    index_digest, index_body = _manifest(zot_registry, unique_mirror_repo, version)
    child_digest = index_body["manifests"][0]["digest"]
    # Not pinned to a number, unlike the keyless leg's 1: this leg is observed
    # to carry a count nothing in the mirror accounts for, and a number nobody
    # can explain is a number the next reader will "correct" in whichever
    # direction their guess runs. Left as non-emptiness deliberately until
    # somebody can say where it comes from. The per-subject and per-run count
    # contracts are `test_a_second_push_adds_one_signature_not_one_per_tag`'s
    # job, on the keyless leg, where the numbers are understood.
    assert _signature_referrers(zot_registry, unique_mirror_repo, index_digest), "the index is unsigned"
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, child_digest)) == 1, (
        f"the {PLATFORM} manifest is unsigned"
    )

    reference = f"{zot_registry}/{unique_mirror_repo}:{version}"
    verified = _verify(real_ocx_binary, signing_mirror, sigstore_stack, reference, "--key", str(COSIGN_PUB))
    assert verified.returncode == 0, (
        f"the signature does not verify against the public half:\n{verified.stdout}\n{verified.stderr}"
    )


# ---------------------------------------------------------------------------
# The unsigned baseline
# ---------------------------------------------------------------------------


def test_a_mirror_without_a_sign_block_publishes_unsigned_and_green(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    real_ocx_binary: Path,
    sigstore_stack,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """No `sign:` is unchanged behaviour — and `verify` says so.

    The control for every test above. Without it, a `verify` that passed
    unconditionally (a stub registry, a permissive matcher, a verify that
    treats "no signature" as vacuously true) would make all three green and
    prove nothing.
    """
    spec_text = signing_spec.read_text()
    head, _, _ = spec_text.partition("sign:")
    signing_spec.write_text(head)

    version, _ = _publish(signing_mirror, signing_spec, tmp_path / "work")

    index_digest, index_body = _manifest(zot_registry, unique_mirror_repo, version)
    child_digest = index_body["manifests"][0]["digest"]
    assert _signature_referrers(zot_registry, unique_mirror_repo, index_digest) == [], (
        "an unsigned mirror attached an index signature — the `sign:` block is not what decides"
    )
    assert _signature_referrers(zot_registry, unique_mirror_repo, child_digest) == [], (
        "an unsigned mirror attached a platform signature"
    )

    reference = f"{zot_registry}/{unique_mirror_repo}:{version}"
    verified = _verify(
        real_ocx_binary,
        signing_mirror,
        sigstore_stack,
        reference,
        "--rekor-url", sigstore_stack.rekor_url,
        "--certificate-identity", SIGSTORE_IDENTITY,
        "--certificate-oidc-issuer", SIGSTORE_ISSUER,
    )
    assert verified.returncode != 0, (
        "verify passed on an unsigned package — every signing assertion in this"
        f" file is vacuous:\n{verified.stdout}\n{verified.stderr}"
    )


# ---------------------------------------------------------------------------
# The in-process Publisher leg
# ---------------------------------------------------------------------------


def test_package_sync_signs_the_leg_it_publishes_in_process(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """D2's third leg: `package sync` publishes through `ocx_lib`, not a subprocess.

    `ocx-mirror package sync` is the only command that reaches
    `push_and_cascade`, and it writes its manifests through the in-process
    `Publisher` — so there is no `ocx package push --sign` to carry the flag
    tail, and every assertion in the tests above would stay green with this
    leg publishing unsigned. The signature is attached afterwards instead, by
    `ocx package sign`: the platform manifest as each push returns, and the
    version's index once its last platform has landed.

    Both are asserted, for the same reason as the pipeline leg: they are two
    call sites in two modules (`pipeline::push` and `pipeline::orchestrator`)
    and either can be missing while the other works.

    This is also the only test that exercises `sync.rs`'s hand-off of the
    resolved `sign:` block into `execute_mirror`. Nothing else routes through
    that call site, so without this the parameter could be dropped and the
    whole suite would stay green.
    """
    signing_mirror.run(
        "package", "sync", str(signing_spec), "--work-dir", str(tmp_path / "sync-work")
    )

    index_digest, index_body = _manifest(zot_registry, unique_mirror_repo, "3.7.0")
    child_digest = index_body["manifests"][0]["digest"]

    # 1 and 1: this leg signs once per platform and once per version, never
    # per tag, so it is already what the sweep should be.
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, child_digest)) == 1, (
        f"the {PLATFORM} manifest is unsigned — `push_and_cascade` did not sign what it published"
    )
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest)) == 1, (
        "the index is unsigned — `execute_mirror` did not sign the version once its platforms landed"
    )


def test_a_second_push_adds_one_signature_not_one_per_tag(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """A second push files one more referrer, not one per cascade tag.

    The guarantee is one referrer per distinct index *per run*. `sign --tags-file`
    resolves tags to digests and signs each subject once, so the five cascade
    tags of a version — all resolving to this one index digest — cost one
    referrer, not five.

    Two, not one, and that is deliberate: `sign` appends. `ocx_lib`'s
    `pipeline.rs` asserts verbatim that a pre-existing signature survives a
    second run, because multi-signature — a second identity joining the first —
    depends on it. Skipping an already-signed subject would break that tested
    behaviour, so idempotence is not the contract and must not be asserted here.
    What the fix bought is the slope: against ocx's cap of 8 candidates, the
    ceiling moves from the second republish to the ninth.

    The count is the assertion, not non-emptiness: non-emptiness is what let
    the five-per-run behaviour go unnoticed in the first place.
    """
    work = tmp_path / "work"
    version, _ = _publish(signing_mirror, signing_spec, work)

    index_digest, _ = _manifest(zot_registry, unique_mirror_repo, version)
    first = len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest))

    # Only the push leg again — same bundles, same JUnit, same index.
    signing_mirror.run(
        "package", "pipeline", "push",
        "--spec", str(signing_spec),
        "--junit-dir", str(work / "junit"),
        "--bundles-dir", str(work / "bundles"),
        "--write-summary", str(work / "run-summary-2.json"),
    )

    second = len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest))
    assert (first, second) == (1, 2), (
        f"the index carries {first} signature(s) after one run and {second} after two —"
        " the sweep is signing once per cascade tag instead of once per subject digest"
    )


# ---------------------------------------------------------------------------
# `pipeline patch` — the republished index
# ---------------------------------------------------------------------------


def test_patch_signs_the_index_it_republishes(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    real_ocx_binary: Path,
    sigstore_stack,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """A metadata patch leaves the tag on a *signed* index, not a bare one.

    `pipeline patch` re-emits each platform manifest through `push --sign`, so
    the platform half looks after itself. The index above them is a different
    subject and its digest changes when they do — so without the closing sweep
    the tag, and every cascade alias re-pointed onto it, resolves to an index
    carrying no signature at all, on a mirror the operator believes is signed.

    Only the registry can witness this: the run summary, the log line and the
    push argv are all identical either way, and an index-level
    `ocx package verify` is the only thing that changes.

    Both digests are asserted:

    - the *new* index must be signed — the fix;
    - the *old* index digest must not be the new one, or the patch re-emitted
      nothing and every assertion below would pass vacuously.
    """
    work = tmp_path / "work"
    version, _ = _publish(signing_mirror, signing_spec, work)

    before_digest, _ = _manifest(zot_registry, unique_mirror_repo, version)
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, before_digest)) == 1

    # The drift a corrected mirror repo would introduce, in the shape
    # `test_mirror_patch.py` uses: one more declared environment entry.
    metadata_path = signing_spec.parent / "metadata.json"
    metadata = json.loads(metadata_path.read_text())
    metadata["env"].append({"key": "SHFMT_ROOT", "type": "constant", "value": "${installPath}"})
    metadata_path.write_text(json.dumps(metadata, indent=2))

    # Deliberately the publish's own warm OCX_HOME — see
    # `test_a_second_run_that_moves_the_index_signs_the_new_one`.
    signing_mirror.run(
        "package", "pipeline", "patch", "--metadata-only", "--spec", str(signing_spec)
    )

    after_digest, after_body = _manifest(zot_registry, unique_mirror_repo, version)
    assert after_digest != before_digest, (
        "the patch re-emitted nothing — every signing assertion below would be vacuous"
    )

    assert len(_signature_referrers(zot_registry, unique_mirror_repo, after_digest)) == 1, (
        "the republished index carries no signature — `pipeline patch` skipped the closing"
        " sweep, so the tag now resolves to an unsigned index on a signed mirror"
    )
    # `push --sign`'s half of D2 still reaches the re-emitted platform manifest.
    child_digest = after_body["manifests"][0]["digest"]
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, child_digest)) == 1, (
        f"the republished {PLATFORM} manifest carries no signature"
    )

    # And what the patch wrote verifies against the stack that issued it.
    verified = _verify(
        real_ocx_binary,
        signing_mirror,
        sigstore_stack,
        f"{zot_registry}/{unique_mirror_repo}:{version}",
        "--rekor-url", sigstore_stack.rekor_url,
        "--certificate-identity", SIGSTORE_IDENTITY,
        "--certificate-oidc-issuer", SIGSTORE_ISSUER,
    )
    assert verified.returncode == 0, f"{verified.stdout}\n{verified.stderr}"


def test_a_second_run_that_moves_the_index_signs_the_new_one(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """Re-running a signed mirror signs the index the re-run actually wrote.

    `ocx package push` records a tag -> digest pin in `$OCX_HOME/index/<registry>
    /p/<repo>.json`, and when a later push moves that tag it refreshes only the
    entry's `observed` timestamp — the `content` digest stays the one the first
    push wrote. A sign child sharing that home then resolves every tag to the
    pre-run index and the sweep fails `manifest not found:
    <repo>@sha256:<old>`, exit 79, for a manifest the registry is serving right
    now. The mirror passes `--remote` so the tag -> manifest lookup goes to the
    registry; digest-addressed reads stay local-first, so no cache the operator
    wants is discarded.

    Re-running a pipeline is an ordinary operator action, and this is the `push`
    leg rather than `patch` because `push` is where the exposure is widest — any
    second run over changed bundles moves the index. It is also why every other
    test in this file is safe on a warm home: they never move one.

    The home is warm by construction: `signing_mirror` sets `OCX_HOME` once and
    both runs inherit it. Nothing here resets it, and resetting it would be the
    bug walking away from its own reproduction.
    """
    work = tmp_path / "work"
    version, _ = _publish(signing_mirror, signing_spec, work)
    first_digest, _ = _manifest(zot_registry, unique_mirror_repo, version)

    # Move the index: a different config blob gives a different platform
    # manifest, which gives a different index digest.
    sidecar = work / "bundles" / f"bundle-{version}-{PLATFORM_SLUG}-metadata.json"
    metadata = json.loads(sidecar.read_text())
    metadata["env"].append({"key": "SHFMT_ROOT", "type": "constant", "value": "${installPath}"})
    sidecar.write_text(json.dumps(metadata, indent=2))

    signing_mirror.run(
        "package", "pipeline", "push",
        "--spec", str(signing_spec),
        "--junit-dir", str(work / "junit"),
        "--bundles-dir", str(work / "bundles"),
        "--write-summary", str(work / "run-summary-2.json"),
    )

    second_digest, _ = _manifest(zot_registry, unique_mirror_repo, version)
    assert second_digest != first_digest, (
        "the second push did not move the index — the signing assertion below would be vacuous"
    )
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, second_digest)) == 1, (
        "the re-pushed index carries no signature — the sweep resolved the tag through ocx's"
        " stale local pin and signed the digest the run replaced"
    )
