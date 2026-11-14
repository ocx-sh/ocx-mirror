# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Structural guards of `release.yml` and `backfill.yml`, ported from ocx's `test_workflows.py`.

Each family is a check function returning violations. The test asserts the live
workflows have none; `CONTROLS` mutates a deep copy of the parsed YAML in memory
and asserts the same function then reds, so a check that stopped reading the
workflows cannot stay green.
"""

from __future__ import annotations

import copy
import re
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest
import yaml

REPO_ROOT = Path(__file__).absolute().parent.parent
WORKFLOWS = REPO_ROOT / ".github" / "workflows"

Docs = dict[str, dict[str, Any]]


def _load(path: Path) -> dict[str, Any]:
    return yaml.safe_load(path.read_text(encoding="utf-8"))


def _documents() -> Docs:
    return {name: _load(WORKFLOWS / name) for name in ("release.yml", "backfill.yml")}


def _jobs(docs: Docs) -> Iterator[tuple[str, dict[str, Any]]]:
    for name, document in docs.items():
        for job_name, job in (document.get("jobs") or {}).items():
            yield f"{name}:{job_name}", job


def _job(docs: Docs, where: str) -> dict[str, Any]:
    name, job = where.split(":")
    return docs[name]["jobs"][job]


def _runs(job: dict[str, Any]) -> list[str]:
    return [str(s["run"]) for s in job.get("steps") or [] if "run" in s]


def _all_runs(docs: Docs) -> list[tuple[str, str]]:
    return [(where, run) for where, job in _jobs(docs) for run in _runs(job)]


def _needs(job: dict[str, Any]) -> list[str]:
    needs = job.get("needs") or []
    return [needs] if isinstance(needs, str) else list(needs)


# ---------------------------------------------------------------------------
# Checks: each returns its violations.
# ---------------------------------------------------------------------------

SIGNERS = {"release.yml:publish", "backfill.yml:backfill"}


def sign_findings(docs: Docs) -> list[str]:
    """Only `publish` and `backfill` sign: the certificate names the file that holds the signing job."""
    found = []
    for where, job in _jobs(docs):
        signs = (job.get("permissions") or {}).get("id-token") == "write" or any(
            re.search(r"--sign\b|package sign\b|--attest\b|package attest\b", run) for run in _runs(job)
        )
        if signs and where not in SIGNERS:
            found.append(f"{where} signs but is not a signing job")
        if where in SIGNERS and not signs:
            found.append(f"{where} no longer signs")
        if signs and "uses" in job:
            found.append(f"{where} signs through a called workflow")
    return found


# Names only the release package or the index; a dev build must not reach them.
_RELEASE_ONLY = re.compile(r"PACKAGE_RELEASE|PACKAGE_LOGICAL|INDEX_REPO|INDEX_PACKAGE|gh release|package announce")


def dev_isolation_findings(docs: Docs) -> list[str]:
    """A dev build changes nothing but the dev package."""
    found = []
    for where, job in _jobs({"release.yml": docs["release.yml"]}):
        condition = str(job.get("if", ""))
        name = where.split(":")[1]
        if name in ("verify", "readiness", "github-release", "announce") and "inputs.mode != 'dev'" not in condition:
            found.append(f"{where} runs on a dev build")
        if name == "prune" and "inputs.mode == 'release'" not in condition:
            found.append(f"{where} is not release-only")
        if name == "prune-dev" and "inputs.mode == 'dev'" not in condition:
            found.append(f"{where} is not dev-only")
        if name not in ("plan", "prune", "prune-dev") and _RELEASE_ONLY.search("\n".join(_runs(job)) + str(job.get("env"))):
            if "inputs.mode != 'dev'" not in condition:
                found.append(f"{where} touches the release package or index without excluding dev")
    release = docs["release.yml"]["jobs"]
    if _RELEASE_ONLY.search("\n".join(_runs(release["prune-dev"]))) or "PACKAGE_RELEASE" in str(release["prune-dev"]):
        found.append("prune-dev names the release package")
    pruned = [s for s in release["prune-dev"]["steps"] if "package prune" in str(s.get("run"))]
    if not pruned or any(s["env"]["PACKAGE"] != "${{ env.PACKAGE_DEV }}" for s in pruned):
        found.append("prune-dev does not prune PACKAGE_DEV")
    plan = "\n".join(_runs(release["plan"]))
    if not re.search(r'dev\)\s.*?package="\$PACKAGE_DEV".*?git_tag=""\s*;;', plan, re.DOTALL):
        found.append("the plan's dev arm does not pick PACKAGE_DEV with an empty git tag")
    return found


def force_findings(docs: Docs) -> list[str]:
    """`--force` only in `prune-dev`, on the dev package."""
    found = []
    for where, job in _jobs(docs):
        for step in job.get("steps") or []:
            for line in str(step.get("run", "")).splitlines():
                if not re.search(r"(?<!\S)--force(?!\S)", line):
                    continue
                if where != "release.yml:prune-dev":
                    found.append(f"{where}: --force outside prune-dev")
                elif '"$PACKAGE"' not in line or step["env"]["PACKAGE"] != "${{ env.PACKAGE_DEV }}":
                    found.append(f"{where}: --force not pinned to the dev package")
    if not any("--force" in run for where, run in _all_runs(docs) if where == "release.yml:prune-dev"):
        found.append("prune-dev lost its --force")
    return found


def prerelease_findings(docs: Docs) -> list[str]:
    """A candidate is a pre-release, a release is the latest."""
    run = "\n".join(_runs(docs["release.yml"]["jobs"]["github-release"]))
    shape = r'if \[\[ "\$MODE" == candidate \]\]; then\s+args\+=\(--prerelease\)\s+else\s+args\+=\(--latest\)\s+fi'
    found = [] if re.search(shape, run) else ["candidate is not --prerelease and release --latest"]
    found += [f"{where} sets a release flag" for where, r in _all_runs(docs) if where != "release.yml:github-release" and re.search(r"--prerelease|--latest", r)]
    return found


def tag_and_release_findings(docs: Docs) -> list[str]:
    """The tag and the release are created once, by `github-release`, after `publish`."""
    runs = _all_runs(docs)
    creators = [where for where, run in runs if "gh release create" in run]
    found = [] if creators == ["release.yml:github-release"] else [f"gh release create in {creators}"]
    found += [f"{where} creates a tag by hand" for where, run in runs if re.search(r"git tag\b|git push\b|git/refs", run)]
    release = docs["release.yml"]["jobs"]["github-release"]
    if "publish" not in _needs(release):
        found.append("github-release does not wait for publish")
    if not any('--target "$GITHUB_SHA"' in run for run in _runs(release)):
        found.append("the release is not created with --target at the dispatched commit")
    return found


def readiness_findings(docs: Docs) -> list[str]:
    """`publish` waits for green `verify` and `readiness`; only a dev build may pass them skipped."""
    publish = docs["release.yml"]["jobs"]["publish"]
    condition = " ".join(str(publish.get("if", "")).split())
    found = [f"publish does not need {n}" for n in ("verify", "readiness") if n not in _needs(publish)]
    if not (
        "needs.verify.result == 'success'" in condition and "needs.readiness.result == 'success'" in condition
    ):
        found.append("publish does not require a green verify and readiness")
    allowed = r"\(inputs\.mode == 'dev' && needs\.verify\.result == 'skipped' && needs\.readiness\.result == 'skipped'\)"
    if condition.count("'skipped'") != 2 or not re.search(allowed, condition):
        found.append("a skipped gate passes publish outside dev mode")
    return found


def identity_findings(docs: Docs) -> list[str]:
    """Release and candidate verify against the literal main identity; no `${{ }}` inside a `run:`."""
    found = [f"{where}: ${{{{ }}}} inside run:" for where, run in _all_runs(docs) if "${{" in run]
    pinned = r"\$\{\{ github\.server_url \}\}/ocx-sh/ocx-mirror/\.github/workflows/release\.yml@refs/heads/main"
    if not re.fullmatch(pinned, str(docs["release.yml"]["env"].get("RELEASE_IDENTITY"))):
        found.append("RELEASE_IDENTITY is not the literal release.yml@refs/heads/main")
    verify = next(
        (s for s in docs["release.yml"]["jobs"]["publish"]["steps"] if "package verify" in str(s.get("run"))), None
    )
    run = str(verify["run"]) if verify else ""
    if not re.search(r'== dev \]\]; then\s+identity="\$\{GITHUB_SERVER_URL\}/\$\{GITHUB_WORKFLOW_REF\}"\s+else\s+identity="\$RELEASE_IDENTITY"\s+fi', run):
        found.append("the verify step does not pick RELEASE_IDENTITY outside dev")
    if run.count("--certificate-identity") != run.count('--certificate-identity "$identity"') or not run:
        found.append("a verify call names an identity other than $identity")
    return found


def status_function_findings(docs: Docs) -> list[str]:
    """A dev-mode job behind a job that dev mode skips needs a status function, or it is skipped too."""
    jobs = docs["release.yml"]["jobs"]

    def ancestors(name: str) -> set[str]:
        direct = _needs(jobs[name])
        return set(direct).union(*(ancestors(n) for n in direct))

    found = []
    for name, job in jobs.items():
        condition = str(job.get("if", ""))
        runs_in_dev = "inputs.mode == 'dev'" in condition
        skippable = any("inputs.mode != 'dev'" in str(jobs[a].get("if", "")) for a in ancestors(name))
        if runs_in_dev and skippable and not ("!cancelled()" in condition and "needs." in condition):
            found.append(f"{name} runs in dev mode behind a skipped ancestor without a status function")
    return found


def concurrency_findings(docs: Docs) -> list[str]:
    release = docs["release.yml"]["concurrency"]
    found = []
    m = re.fullmatch(r"(\w+)-\$\{\{ inputs\.mode == 'dev' && '(\w+)' \|\| '(\w+)' \}\}", str(release["group"]))
    if not m:
        found.append("the release group no longer selects by dev mode")
    else:
        prefix, dev, other = m.groups()
        if dev == other:
            found.append("a dev build shares the release group")
        # Release and candidate take the same arm, so they share a group by construction.
    if release.get("cancel-in-progress") is not False:
        found.append("release cancels in progress")
    backfill = docs["backfill.yml"]["concurrency"]
    if backfill.get("group") != "backfill" or backfill.get("cancel-in-progress") is not False:
        found.append("backfill group or cancel-in-progress changed")
    return found


def floating_tag_findings(docs: Docs) -> list[str]:
    """Floating tags move once, after every platform is pushed, signed, attested and verified."""
    steps = docs["release.yml"]["jobs"]["publish"]["steps"]

    def index(pred: Callable[[str], bool]) -> int:
        hits = [i for i, s in enumerate(steps) if pred(str(s.get("run", "")))]
        return hits[0] if len(hits) == 1 else -1

    push = index(lambda r: "publish_mirror_package.sh" in r and "--sign" in r)
    verify = index(lambda r: "package verify" in r and "--attestation" in r)
    move = index(lambda r: "cascade repair" in r)
    found = []
    if min(push, verify, move) < 0:
        found.append("publish lost its push, verify or cascade repair step (or has two)")
    elif not push < verify < move:
        found.append("the floating tags move before every platform is pushed, signed and verified")
    for where, run in _all_runs(docs):
        if re.search(r"--cascade\b", re.sub(r"(?m)^\s*#.*$", "", run)):
            found.append(f"{where}: --cascade moves a floating tag at push time")
        if "cascade repair" in run and where != "release.yml:publish":
            found.append(f"{where}: cascade repair outside publish")
    return found


def prune_reconcile_findings(docs: Docs) -> list[str]:
    """The prune reconciliation only removes rows and still runs, and fails the job, after a failed prune."""
    steps = docs["release.yml"]["jobs"]["prune"]["steps"]
    prune = next(s for s in steps if s.get("id") == "prune")
    announce = next(s for s in steps if "package announce" in str(s.get("run")))
    run, condition = str(announce["run"]), str(announce.get("if"))
    found = []
    if not re.search(r"(?<!\S)--refresh(?!\S)", run) or "--tags-from-registry" in run:
        found.append("the reconciliation is not --refresh only")
    if "!cancelled()" not in condition or "steps.prune.conclusion != 'skipped'" not in condition or "outputs.pruned" in condition:
        found.append("the reconciliation does not run after a failed prune")
    if prune.get("continue-on-error"):
        found.append("continue-on-error masks a failed prune")
    return found


CHECKS: dict[str, Callable[[Docs], list[str]]] = {
    "sign": sign_findings,
    "dev-isolation": dev_isolation_findings,
    "force": force_findings,
    "prerelease": prerelease_findings,
    "tag-and-release": tag_and_release_findings,
    "readiness": readiness_findings,
    "identity": identity_findings,
    "status-function": status_function_findings,
    "concurrency": concurrency_findings,
    "floating-tags": floating_tag_findings,
    "prune-reconcile": prune_reconcile_findings,
}


@pytest.mark.parametrize("family", CHECKS)
def test_the_live_workflows_hold_the_guard(family: str) -> None:
    assert CHECKS[family](_documents()) == []


# ---------------------------------------------------------------------------
# Negative controls: one mutation per family, on a deep copy in memory.
# ---------------------------------------------------------------------------


def _replace_run(docs: Docs, where: str, old: str, new: str) -> None:
    landed = False
    for step in _job(docs, where).get("steps") or []:
        if old in str(step.get("run", "")):
            step["run"] = step["run"].replace(old, new)
            landed = True
    assert landed, f"mutation did not land: {old!r} is in no step of {where}"


def _replace_if(docs: Docs, where: str, old: str, new: str) -> None:
    job = _job(docs, where)
    assert old in job["if"], f"mutation did not land: {old!r} not in the if of {where}"
    job["if"] = job["if"].replace(old, new)


def _move_cascade_before_verify(docs: Docs) -> None:
    steps = _job(docs, "release.yml:publish")["steps"]
    move = next(i for i, s in enumerate(steps) if "cascade repair" in str(s.get("run")))
    steps.insert(move - 2, steps.pop(move))


CONTROLS: dict[str, Callable[[Docs], None]] = {
    "sign": lambda d: _job(d, "release.yml:github-release").setdefault("permissions", {}).update({"id-token": "write"}),
    "dev-isolation": lambda d: _replace_if(d, "release.yml:announce", "inputs.mode != 'dev'", "true"),
    "force": lambda d: _replace_run(d, "release.yml:prune", "package prune --forge github", "package prune --forge github --force"),
    "prerelease": lambda d: _replace_run(d, "release.yml:github-release", "args+=(--prerelease)", "args+=(--latest)"),
    "tag-and-release": lambda d: _job(d, "release.yml:github-release").update(needs=["plan"]),
    "readiness": lambda d: _replace_if(d, "release.yml:publish", "needs.readiness.result == 'success'", "true"),
    "identity": lambda d: _replace_run(d, "release.yml:publish", 'identity="$RELEASE_IDENTITY"', 'identity="${{ github.workflow_ref }}"'),
    "status-function": lambda d: _replace_if(d, "release.yml:prune-dev", "!cancelled() && ", ""),
    "concurrency": lambda d: d["release.yml"]["concurrency"].update(group="release-${{ inputs.mode == 'dev' && 'release' || 'release' }}"),
    "floating-tags": _move_cascade_before_verify,
    "prune-reconcile": lambda d: _replace_run(d, "release.yml:prune", "announce --refresh", "announce --refresh --tags-from-registry"),
}


def test_every_family_has_a_control() -> None:
    assert CONTROLS.keys() == CHECKS.keys()


@pytest.mark.parametrize("family", CHECKS)
def test_the_control_reds_the_guard(family: str) -> None:
    docs = _documents()
    mutated = copy.deepcopy(docs)
    CONTROLS[family](mutated)
    assert mutated != docs, "the mutation changed nothing"
    assert CHECKS[family](mutated), f"{family}: the check stayed green on the mutation"


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q", "-p", "no:cacheprovider"]))
