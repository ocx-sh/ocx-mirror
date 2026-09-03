# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Acceptance for the signature backfill (WP 4).

Contract source: ``C-067``–``C-070``, ``C-074`` and scenarios ``S-056``,
``S-057`` in ``.claude/state/plans/plan_mirror_signing.md``.

The command under test is ``ocx-mirror package pipeline sign``: the one leg of
``adr_mirror_signing.md`` D2 that signs content **already published**, which is
everything a mirror pushed before ``sign:`` reached its spec. So every test
here publishes with the ``sign:`` block cut out of the fixture, confirms the
registry holds nothing, and only then runs the backfill.

**The registry is the witness**, for the same reason ``test_signing.py`` gives:
``ocx package verify --platform`` exits 0 on an index whose *index* is signed
even when the platform manifest carries none, so it cannot answer the
per-subject question this command's filter asks. Every assertion below counts
signature referrers for one specific subject digest.

**Why the cascade matters here.** The fixture publishes with ``cascade: true``,
so ``3.7.0``, ``3.7``, ``3`` and ``latest`` are four tags at one index digest.
A backfill keying its skip on the *tag* would file four referrers against that
one subject and burn half of ocx's eight-candidate verifier budget in a single
run — which is what ``one_run_signs_each_distinct_subject_once`` and
``S-057``'s repeated-run count exist to catch.
"""
from __future__ import annotations

import json
import signal
import subprocess
import time
from pathlib import Path

import pytest

from test_signing import (
    PLATFORM,
    SIGSTORE_IDENTITY,
    SIGSTORE_ISSUER,
    _manifest,
    _publish,
    _signature_referrers,
    _verify,
    # Re-exported, not merely referenced: pytest resolves a fixture by name in
    # the requesting module's namespace, so a fixture defined in a sibling test
    # module has to be imported here to be reachable at all. WP 2 owns both,
    # and duplicating them would be two harnesses drifting against one stack.
    signing_mirror,  # noqa: F401
    signing_spec,  # noqa: F401
)
from src.mirror_runner import MirrorRunner


def _subjects(registry: str, repository: str, version: str) -> tuple[str, str]:
    """The ``(index digest, linux/amd64 manifest digest)`` for ``version``.

    Both read back off the registry rather than computed here: they are the
    identities every referrer is filed under, and hashing locally would be the
    test agreeing with itself about canonical bytes.
    """
    index_digest, index = _manifest(registry, repository, version)
    platform_digest = next(
        entry["digest"]
        for entry in index["manifests"]
        if f"{entry['platform']['os']}/{entry['platform']['architecture']}" == PLATFORM
    )
    return index_digest, platform_digest


def _backfill(mirror: MirrorRunner, spec: Path, *extra: str, check: bool = True) -> dict:
    """Run the backfill and return the parsed envelope (C-068).

    Parsing the *whole* of stdout, not a fragment of it: ``--format json``
    promises the payload and nothing else, so a banner or a progress line
    would fail here rather than being tolerated.
    """
    result = mirror.run(
        "package", "pipeline", "sign", str(spec), "--format", "json", *extra, check=check
    )
    return json.loads(result.stdout)


@pytest.fixture()
def unsigned_spec(signing_spec: Path) -> Path:
    """The signing fixture with its ``sign:`` block cut off.

    Publishing through this spec is what puts the registry into the state the
    backfill exists for. Everything above ``sign:`` is untouched, so the two
    specs name the same target repository and the same version — the backfill
    reads back exactly what this published.
    """
    text = signing_spec.read_text()
    head, marker, _ = text.partition("\nsign:\n")
    assert marker, "the signing fixture no longer carries a `sign:` block to cut"
    unsigned = signing_spec.parent / "mirror-unsigned.yml"
    unsigned.write_text(head + "\n")
    return unsigned


@pytest.fixture()
def published_unsigned(
    signing_mirror: MirrorRunner,
    unsigned_spec: Path,
    tmp_path: Path,
    zot_registry: str,
    unique_mirror_repo: str,
) -> tuple[str, str, str]:
    """Publish one version unsigned; return ``(version, index, platform)``.

    The zero-referrer assertion is not decoration: without it every count
    below could be satisfied by a publish that had already signed, and the
    whole suite would pass against a backfill that does nothing.
    """
    version, _ = _publish(signing_mirror, unsigned_spec, tmp_path / "publish")
    index_digest, platform_digest = _subjects(zot_registry, unique_mirror_repo, version)

    assert _signature_referrers(zot_registry, unique_mirror_repo, index_digest) == []
    assert _signature_referrers(zot_registry, unique_mirror_repo, platform_digest) == []
    return version, index_digest, platform_digest


# ---------------------------------------------------------------------------
# S-056 — signs exactly the unsigned subjects
# ---------------------------------------------------------------------------


def test_the_backfill_signs_the_index_and_the_platform(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
    zot_registry: str,
    unique_mirror_repo: str,
) -> None:
    """Both halves of D2 get a signature, and the report says so (S-056)."""
    _, index_digest, platform_digest = published_unsigned

    envelope = _backfill(signing_mirror, signing_spec)

    summary = envelope["summary"]
    assert summary["status"] == "success", envelope
    assert summary["exit_code"] == 0
    assert summary["succeeded"] == summary["total"]
    assert summary["failed"] == 0

    assert len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest)) == 1
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, platform_digest)) == 1


def test_one_run_signs_each_distinct_subject_once(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
) -> None:
    """Four cascade tags at one index digest are two subjects, not five.

    ``3.7.0``, ``3.7``, ``3`` and ``latest`` all resolve to the same index, so
    a correct run considers exactly two items: that index and its one platform
    manifest. A tag-keyed implementation reports five.
    """
    envelope = _backfill(signing_mirror, signing_spec)

    assert envelope["summary"]["total"] == 2, envelope["items"]
    subjects = {item["subject"] for item in envelope["items"]}
    assert len(subjects) == 2, envelope["items"]
    # One index row (`platform: null`) and one platform row — the two passes.
    assert sorted(item["platform"] is None for item in envelope["items"]) == [False, True]


def test_a_second_run_signs_nothing(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
    zot_registry: str,
    unique_mirror_repo: str,
) -> None:
    """Convergence: the re-run skips every subject and exits 0 (S-056).

    ``ocx package sign`` appends by design, so nothing downstream would stop a
    second referrer landing — the skip is this command's own, and the referrer
    count is the only thing that can prove it happened.
    """
    _, index_digest, platform_digest = published_unsigned
    _backfill(signing_mirror, signing_spec)

    second = _backfill(signing_mirror, signing_spec)

    assert second["summary"]["status"] == "success"
    assert second["summary"]["exit_code"] == 0
    assert second["summary"]["skipped"] == second["summary"]["total"] == 2
    assert second["summary"]["succeeded"] == 0
    assert {item["reason"] for item in second["items"]} == {"already_signed"}
    # Every skip names the evidence it skipped on. Nothing else exercises the
    # `discovery` field's population — the envelope unit test builds its rows
    # by hand — so a skip that stopped reporting how it decided would
    # otherwise be silent.
    assert all(item.get("discovery") for item in second["items"]), second["items"]
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest)) == 1
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, platform_digest)) == 1


def test_force_signs_an_already_signed_subject(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
    zot_registry: str,
    unique_mirror_repo: str,
) -> None:
    """``--force`` is how a second identity joins an existing signature (S-056).

    Paired with ``test_a_second_run_signs_nothing`` on purpose: together they
    show the skip is a decision and not an inability to sign twice.
    """
    _, index_digest, platform_digest = published_unsigned
    _backfill(signing_mirror, signing_spec)

    forced = _backfill(signing_mirror, signing_spec, "--force")

    assert forced["summary"]["succeeded"] == 2, forced["items"]
    assert forced["summary"]["skipped"] == 0
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest)) == 2
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, platform_digest)) == 2


# ---------------------------------------------------------------------------
# C-067 — --identity / --issuer narrow what counts as already signed
# ---------------------------------------------------------------------------


def test_identity_narrowing_reads_the_signer_off_the_live_signature(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
    zot_registry: str,
    unique_mirror_repo: str,
) -> None:
    """The narrowing flags match against a real Fulcio certificate (C-067).

    Nothing in ``cargo nextest`` can prove this half. The unit tests decide the
    filter from candidate values handed to them; whether the certificate SAN
    and the Fulcio issuer extension of a signature this stack just wrote ever
    *reach* those values is a property of the whole path — bundle blob,
    certificate parse, upstream listing, the mirror-side mapping — and only a
    live signature exercises it. A filter comparing against two permanently
    ``None`` fields would pass every unit test and skip nothing here.

    The three verdicts, in order: the pair that signed it skips, one wrong half
    does not, and a foreign identity re-signs. The two dry runs are free —
    ``--dry-run`` reports the verdict and issues no child — so only the last
    pass costs a Fulcio and Rekor round trip.
    """
    _, index_digest, platform_digest = published_unsigned
    _backfill(signing_mirror, signing_spec)

    matching = _backfill(
        signing_mirror,
        signing_spec,
        "--dry-run",
        "--identity",
        SIGSTORE_IDENTITY,
        "--issuer",
        SIGSTORE_ISSUER,
    )
    assert matching["summary"]["skipped"] == matching["summary"]["total"] == 2
    assert {item["reason"] for item in matching["items"]} == {"already_signed"}, matching["items"]

    # The identity still matches, so a `dry_run` verdict here can only come
    # from the issuer half being read and compared.
    wrong_issuer = _backfill(
        signing_mirror,
        signing_spec,
        "--dry-run",
        "--identity",
        SIGSTORE_IDENTITY,
        "--issuer",
        "https://issuer.invalid",
    )
    assert {item["reason"] for item in wrong_issuer["items"]} == {"dry_run"}, wrong_issuer["items"]

    # The rotation case, for real: somebody else's signature does not count as
    # this signer's, so every subject is signed again and the existing
    # signature survives beside the new one.
    rotated = _backfill(signing_mirror, signing_spec, "--identity", "rotated@example.com")
    assert rotated["summary"]["succeeded"] == rotated["summary"]["total"] == 2, rotated["items"]
    assert rotated["summary"]["skipped"] == 0
    for digest in (index_digest, platform_digest):
        assert len(_signature_referrers(zot_registry, unique_mirror_repo, digest)) == 2


# ---------------------------------------------------------------------------
# S-057 — repeated runs stay inside the verifier's candidate budget
# ---------------------------------------------------------------------------


def test_repeated_runs_stay_within_the_candidate_budget(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
    real_ocx_binary: Path,
    sigstore_stack,
    zot_registry: str,
    unique_mirror_repo: str,
) -> None:
    """Three backfills and one ``--force`` leave two candidates, and verify passes.

    ocx's verifier caps signature candidates at eight, so an implementation
    that re-signed on every pass would cross the ceiling on a repository swept
    nightly and start dropping candidates silently. Two is the count a
    deliberate second signing produces; anything above it is the skip failing.
    """
    version, index_digest, platform_digest = published_unsigned

    for _ in range(3):
        _backfill(signing_mirror, signing_spec)
    _backfill(signing_mirror, signing_spec, "--force")

    for digest in (index_digest, platform_digest):
        assert len(_signature_referrers(zot_registry, unique_mirror_repo, digest)) == 2

    # The identity flags are not optional: `verify` refuses a keyless
    # signature it has no trust policy for, so omitting them fails with a
    # usage error that says nothing about the two candidates just counted.
    verified = _verify(
        real_ocx_binary,
        signing_mirror,
        sigstore_stack,
        f"{zot_registry}/{unique_mirror_repo}:{version}",
        "--certificate-identity",
        SIGSTORE_IDENTITY,
        "--certificate-oidc-issuer",
        SIGSTORE_ISSUER,
    )
    assert verified.returncode == 0, verified.stderr


# ---------------------------------------------------------------------------
# C-070 — --dry-run reports and signs nothing
# ---------------------------------------------------------------------------


def test_dry_run_reports_a_verdict_and_issues_no_child(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
    zot_registry: str,
    unique_mirror_repo: str,
) -> None:
    """Every subject is reported, and the registry is untouched (C-070)."""
    _, index_digest, platform_digest = published_unsigned

    envelope = _backfill(signing_mirror, signing_spec, "--dry-run")

    assert envelope["summary"]["status"] == "success"
    assert envelope["summary"]["skipped"] == envelope["summary"]["total"] == 2
    assert {item["reason"] for item in envelope["items"]} == {"dry_run"}
    assert _signature_referrers(zot_registry, unique_mirror_repo, index_digest) == []
    assert _signature_referrers(zot_registry, unique_mirror_repo, platform_digest) == []


# ---------------------------------------------------------------------------
# PKG-27 — SIGINT mid-batch drains into a cancelled report
# ---------------------------------------------------------------------------


# Long enough that the tokio signal handler is armed — it is installed at the
# start of the batch, after the spec load, the tag listing and the per-tag
# manifest fetches — and short enough that signing is still in flight. A
# signal arriving before the handler kills the process outright, which the
# returncode assertion below reports as itself rather than as a lost report.
_SIGINT_DELAY_SECONDS = 1.0


def _backfill_interrupted(
    mirror: MirrorRunner, spec: Path, *extra: str
) -> tuple[dict, str, int]:
    """``SIGINT`` a running backfill; return ``(envelope, stderr, returncode)``.

    ``Popen`` rather than :meth:`MirrorRunner.run`, which blocks: the whole
    point is to signal a process that is still running. ``stderr`` comes back
    because the human-facing half of the cancellation contract lives there —
    the run exits 0, so a plain-text user has nowhere else to learn it stopped.
    The code comes back because that 0 is itself the pinned decision.
    """
    process = subprocess.Popen(  # noqa: S603
        [str(mirror.binary), "package", "pipeline", "sign", str(spec), "--format", "json", *extra],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=mirror.env,
        cwd=str(mirror.temp_dir),
    )
    time.sleep(_SIGINT_DELAY_SECONDS)
    process.send_signal(signal.SIGINT)
    stdout, stderr = process.communicate(timeout=120)

    assert process.returncode >= 0, (
        f"the run died of the signal (rc={process.returncode}) instead of catching it; "
        f"raise _SIGINT_DELAY_SECONDS past the handler's arming\nstderr: {stderr.strip()}"
    )
    return json.loads(stdout), stderr, process.returncode


def test_sigint_mid_batch_accounts_for_every_subject(
    signing_mirror: MirrorRunner,
    signing_spec: Path,
    published_unsigned: tuple[str, str, str],
) -> None:
    """An interrupted pass reports every subject, none silently absent (PKG-27).

    The accounting is the property. A run that dropped the subjects it never
    reached would be indistinguishable from a complete run over a smaller
    repository — the one failure an operator cannot see, and the reason this
    asserts on the item set rather than on the exit code.

    ``--force`` is what makes the window reliable: it re-signs subjects that
    already carry a signature, so both children do full Fulcio and Rekor work
    rather than being skipped in microseconds.
    """
    # Sign first, so the interrupted run below has real work for `--force` to
    # redo rather than a filter verdict to return immediately.
    baseline = _backfill(signing_mirror, signing_spec)
    expected = {(item["tag"], item["platform"]) for item in baseline["items"]}
    assert len(expected) == 2, baseline["items"]

    envelope, stderr, returncode = _backfill_interrupted(signing_mirror, signing_spec, "--force")
    summary, items = envelope["summary"], envelope["items"]

    # Every target the pass planned is present exactly once.
    assert {(item["tag"], item["platform"]) for item in items} == expected, items
    assert len(items) == summary["total"] == 2, items

    # `cancelled` is its own verdict, not folded into partial_failure.
    #
    # `.get`, not `[...]`: `reason` is omitted on a row that has none, so a
    # subject that finished before the signal would raise KeyError here and
    # report a timing accident as an error rather than as the assertion that
    # actually failed.
    assert summary["status"] == "cancelled", envelope
    assert any(item.get("reason") == "cancelled" for item in items), items

    # An unattempted subject is skipped, never failed: no child ran, so
    # nothing about it is known to be wrong.
    for item in items:
        if item.get("reason") == "cancelled":
            assert item["status"] == "skipped", item
    assert summary["failed"] == 0, items

    # The pinned decision: an interrupted pass that reached no failure exits
    # **0**, and `summary.exit_code` mirrors the process code. Asserted here
    # because this is the only place either is observable.
    assert summary["exit_code"] == 0, envelope
    assert returncode == 0, f"rc={returncode}\nstderr: {stderr.strip()}"

    # The human half: exit 0 means stderr is the only place a plain-text user
    # learns the run stopped early, so it must say so and say how much it
    # missed. Asserting the count, not just the word, so a message that
    # drifted out of step with the report fails here.
    unattempted = sum(1 for item in items if item.get("reason") == "cancelled")
    assert "interrupted" in stderr, stderr
    assert f"{unattempted} of {summary['total']} subjects" in stderr, stderr
