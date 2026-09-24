#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Every test result in a build event stream was served from a cache (C-007).

    scripts/bazel_cache_check.py --bep <bep.json>
    scripts/bazel_cache_check.py --self-test

`task bazel:cache:check` re-runs the test targets on an unchanged tree and
hands the BEP here; any target that executed again is a finding — an
undeclared input or a non-hermetic action.

A `TestResult` is **cached iff** `cachedLocally` is true (the live server's
action cache) **or** `executionInfo.strategy` names a cache hit (`disk cache
hit`, `remote cache hit`). **Never `cachedRemotely` alone**: on Bazel 9.2.0 it
is set for a `--disk_cache` hit too, and a reader that trusts it without the
strategy calls a record green that no runner vouched for (ocx's reader accepts
it; the mirror's contract does not). Measured tiers, from
`tests/fixtures/bep/cache_states.json` (four real runs of one workspace):

    run                       strategy            cachedLocally  cachedRemotely
    executed                  linux-sandbox       absent         absent
    action cache (live)       (executionInfo {})  true           absent
    --disk_cache hit          disk cache hit      absent         true
    remote cache hit          remote cache hit    absent         true

proto3 JSON omits a false boolean, so truth is read, never presence. A label
with several attempts is cached only if every attempt was.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path

from _gate import expect

# `absolute()`, never `resolve()`: under `bazel test` (scripts/BUILD.bazel) this
# file is a runfiles symlink into the source tree, and resolving it would let
# the self-test read the checkout instead of its declared inputs.
REPO_ROOT = Path(__file__).absolute().parent.parent
FIXTURE = REPO_ROOT / "tests" / "fixtures" / "bep" / "cache_states.json"


def _truthy(value: object) -> bool:
    return value is True or (isinstance(value, str) and value.lower() == "true")


def evidence(payload: dict) -> str:
    """Why this `TestResult` counts as cached, or `""` for "it ran"."""
    info = payload.get("executionInfo") or {}
    strategy = info.get("strategy") if isinstance(info, dict) else None
    if isinstance(strategy, str) and "cache hit" in strategy.lower():
        return f"strategy={strategy}"
    if _truthy(payload.get("cachedLocally")):
        return "cachedLocally"
    return ""


def read_results(text: str) -> tuple[list[tuple[str, dict]], int]:
    """`[(label, payload)]` for every `TestResult` event, plus the malformed count."""
    results: list[tuple[str, dict]] = []
    malformed = 0
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            malformed += 1  # a truncated stream reads as a smaller universe
            continue
        identifier = event.get("id", {}).get("testResult")
        payload = event.get("testResult")
        if not isinstance(identifier, dict) or not isinstance(payload, dict):
            continue
        label = identifier.get("label")
        if not isinstance(label, str) or not label:
            malformed += 1
            continue
        results.append((label, payload))
    return results, malformed


def check(text: str) -> tuple[list[str], Counter[str]]:
    """Findings, and the per-evidence tally of every attempt."""
    results, malformed = read_results(text)
    cached: dict[str, bool] = {}
    tally: Counter[str] = Counter()
    for label, payload in results:
        why = evidence(payload)
        info = payload.get("executionInfo") or {}
        tally[why or f"executed ({info.get('strategy', 'no strategy') if isinstance(info, dict) else '?'})"] += 1
        cached[label] = cached.get(label, True) and bool(why)
    findings = [f"bazel cache check: {label} re-executed on an unchanged tree" for label, ok in sorted(cached.items()) if not ok]
    if malformed:
        findings.append(f"bazel cache check: {malformed} unparseable or label-less event(s) — a truncated BEP")
    if not cached:
        findings.append("bazel cache check: no `testResult` event — nothing was judged, which is not a pass")
    return findings, tally


def run(bep: Path) -> int:
    try:
        text = bep.read_text(encoding="utf-8")
    except OSError as error:
        print(f"bazel cache check: cannot read {bep}: {error}", file=sys.stderr)
        return 1
    findings, tally = check(text)
    print("bazel cache check: " + ", ".join(f"{name}: {count}" for name, count in sorted(tally.items())))
    sys.stdout.flush()
    for finding in findings:
        print(finding, file=sys.stderr)
    if not findings:
        print(f"bazel cache check: all {sum(tally.values())} test result(s) served from a cache")
    return 1 if findings else 0


def _event(label: str, payload: dict) -> str:
    return json.dumps({"id": {"testResult": {"label": label, "run": 1, "shard": 1, "attempt": 1}}, "testResult": payload})


def self_test() -> int:
    results, malformed = read_results(FIXTURE.read_text(encoding="utf-8"))
    expect(not malformed and len(results) == 12, f"{FIXTURE.name}: 4 runs x 3 targets expected, got {len(results)}")
    # The fixture is four runs in order; each label appears once per run.
    runs: list[list[str]] = [[], [], [], []]
    seen: Counter[str] = Counter()
    for label, payload in results:
        runs[seen[label]].append(_event(label, payload))
        seen[label] += 1
    names = ["executed", "action cache (cachedLocally)", "disk cache hit", "remote cache hit"]
    for name, lines, want_green in zip(names, runs, [False, True, True, True], strict=True):
        findings, tally = check("\n".join(lines))
        expect(bool(findings) != want_green, f"{name}: expected {'green' if want_green else 'red'}, got {findings}")
        print(f"{'GREEN' if want_green else 'RED  '} : {name:30} {dict(tally)}")

    # Planted: cachedRemotely with no cache-hit strategy — the disk-cache false green.
    planted = {"status": "PASSED", "executionInfo": {"strategy": "linux-sandbox", "cachedRemotely": True}}
    findings, _ = check(_event("//:planted", planted))
    expect(findings == ["bazel cache check: //:planted re-executed on an unchanged tree"], f"cachedRemotely alone must red, got {findings}")
    print("RED   : cachedRemotely true, strategy linux-sandbox -> not cached")

    # An explicit false is not a hit; presence is never read.
    findings, _ = check(_event("//:false", {"cachedLocally": False, "executionInfo": {"strategy": "linux-sandbox"}}))
    expect(len(findings) == 1, "an explicit cachedLocally: false must red")
    # Mixed attempts: cached only if every attempt was.
    mixed = "\n".join([runs[1][0], runs[0][0]])
    findings, _ = check(mixed)
    expect(len(findings) == 1, f"one re-executed attempt must red the label, got {findings}")
    # Empty and truncated streams.
    expect(check("")[0] != [], "an empty stream must red")
    expect(any("unparseable" in f for f in check(runs[1][0] + "\n{\"id\": {")[0]), "a truncated stream must red")
    print("RED   : explicit false, a re-executed attempt, an empty and a truncated stream")
    print("bazel cache check self-test: executed + cachedRemotely-only red; cachedLocally, disk and remote hits green")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--bep", type=Path)
    args = parser.parse_args()
    return self_test() if args.self_test else run(args.bep)


if __name__ == "__main__":
    raise SystemExit(main())
