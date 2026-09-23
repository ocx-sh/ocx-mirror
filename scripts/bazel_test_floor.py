#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""The Bazel unit-test floor, per-case JUnit, and the cargo/Bazel coverage gate.

    scripts/bazel_test_floor.py --self-test
    scripts/bazel_test_floor.py --bep <bep.json> [--junit target/bazel/junit.xml] [--update]
    scripts/bazel_test_floor.py --bep <bep.json> --coverage <nextest-list.json>

Port of ocx's `scripts/bazel_test_floor.py` (adr_bazel_crate_split.md § C4, C7;
plan C-006, C-009). The helpers ocx imports from `bazel_gate_proofs` (`Finding`,
`codes`, `expect`, `report`) are inlined. Differences from ocx:

* **No constant target floor and no skip ceiling.** The reader floor is the
  row count of `crates/TEST_TARGET_MAP.toml`; the mirror has no `#[ignore]`d
  case to cap.
* **The count check runs with or without `--junit`.** Per target, the
  per-case lines libtest printed must number exactly `passed + failed +
  ignored + measured` from its `test result:` line — two readings of one log;
  a libtest format change moves them apart and reds.
* **A `TestResult` with no `test.log`** reds (`floor-no-output`) instead of
  being dropped. `test.xml` is never read: Bazel's synthesised one carries one
  `testcase` per *target*, so a cached target whose `test.xml` was not
  downloaded still gets its per-case JUnit, rebuilt from the log.
* `--update` rewrites the map from a run: counts rise only.
* `--coverage` is C4's gate: every case `cargo nextest list` names is
  executed by a Bazel target. There is no exclusion list any more (the
  `[[excluded]]` rows it once honoured all moved under Bazel, and nothing
  else runs a skipped case), so a map still carrying one reds as
  `map-excluded`.

Why `test.log` and not `test.xml`, and why per target rather than a sum: see
ocx's copy of this file; both measured there and unchanged here.

Stdlib only; `xml.etree.ElementTree` writes the report (escaping is not ours).
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import re
import tempfile
import tomllib
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

from _gate import Finding, codes, expect, report

REPO_ROOT = Path(__file__).resolve().parent.parent
TEST_TARGET_MAP = REPO_ROOT / "crates" / "TEST_TARGET_MAP.toml"

#: libtest's summary line. `executed` is `passed + failed`: a failing test ran.
RESULT_LINE = re.compile(
    r"^test result: \w+\. (?P<passed>\d+) passed; (?P<failed>\d+) failed; "
    r"(?P<ignored>\d+) ignored; (?P<measured>\d+) measured; (?P<filtered>\d+) filtered out",
    re.MULTILINE,
)

#: libtest's per-case line. The summary line carries no ` ... `, so the two
#: readings stay independent.
CASE_LINE = re.compile(r"^test (?P<name>\S[^\n]*?) \.\.\. (?P<outcome>[^\n]+?)[ \t]*$", re.MULTILINE)

#: A failing case's captured output (the panic), up to the next block or the
#: trailing `failures:` roll-up. No `--test_env` is passed by any task, so a
#: test runs with Bazel's static environment and no secret is in scope here.
FAILURE_BLOCK = re.compile(
    r"^---- (?P<name>[^\n]+?) (?:stdout|stderr) ----\n(?P<body>.*?)(?=^---- |^failures:$|\Z)",
    re.MULTILINE | re.DOTALL,
)

#: What XML 1.0 admits. libtest copies raw stdout (ANSI colour and all).
XML_FORBIDDEN = re.compile("[^\\x09\\x0a\\x0d\\x20-\\ud7ff\\ue000-\\ufffd\\U00010000-\\U0010ffff]")

PASSING_STATUS = frozenset({"PASSED", "FLAKY"})

MAP_HEADER = """\
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
#
# adr_bazel_crate_split.md § C4, C7; plan C-006, C-009. One `[[target]]` row
# per Bazel `rust_test`, with the testcases `bazel test` last executed for it.
#
# `[[target]]` rows are generated — `task bazel:test:unit FLOOR_ARGS=--update`
# rewrites them from the run's `test.log`s. **Counts rise only**: a falling
# count is a deleted test or a `--skip` that widened past its name; lower it by
# hand in the same commit and say why in its body.
#
#   suite    the `cargo nextest list` binary id the target mirrors
#   cases    executed (passed + failed); excludes `--skip`ped ones
#   skipped  how many the target's `args` filter out (libtest `filtered out`)
#
# There are no `[[excluded]]` rows: every case `cargo nextest list` names runs
# under Bazel, and `task bazel:test:coverage` proves it (nothing else would run
# a case a target `--skip`s). The last four moved in the full Bazel adoption.
"""

EMPTY_MSG = (
    "bazel test floor: {path} holds no `testResult` event — check that `bazel test` ran "
    "with `--build_event_json_file`; an export of nothing is not a success"
)
READER_MSG = (
    "bazel test floor: read {read} test target(s) from the build event stream, "
    "crates/TEST_TARGET_MAP.toml has {rows} rows — the run selected a partial graph"
)
NO_OUTPUT_MSG = (
    "bazel test floor: {label} reported no test.log (and no test.xml is read) — its cases "
    "cannot be counted or reported. Under --remote_download_minimal keep "
    "--remote_download_regex matching test.log"
)
UNREADABLE_MSG = (
    "bazel test floor: {label} has no libtest `test result:` line in {path} — its count is "
    "unknown, which is not the same as its count being high enough"
)
MISSING_MSG = "bazel test floor: {label} is in crates/TEST_TARGET_MAP.toml and absent from the run"
SHRANK_MSG = (
    "bazel test floor: {label} executed {observed} case(s), crates/TEST_TARGET_MAP.toml "
    "records {recorded} — tests were removed or a --skip widened. Counts rise only; lower the "
    "row by hand in the same commit and say why"
)
COUNT_MSG = (
    "bazel test floor: {label} yields {written} `test <name> ... <outcome>` line(s) and its "
    "libtest summary records {expected} (passed + failed + ignored + measured) — two readings "
    "of one log that disagree; fix CASE_LINE, do not lower this"
)
UNLISTED_MSG = (
    "bazel test floor: {label} ran but has no crates/TEST_TARGET_MAP.toml row — add it with "
    "`task bazel:test:unit FLOOR_ARGS=--update`"
)
MAP_UNREADABLE_MSG = "bazel test floor: {path} does not read as TOML ({error}) — nothing is judged or rewritten"
MAP_EXCLUDED_MSG = (
    "bazel test floor: {path} carries an [[excluded]] row — nothing runs a case a Bazel "
    "target skips any more; run it under Bazel (its own rust_test, declared data) and delete the row"
)
JUNIT_EMPTY_MSG = "bazel junit: not one testcase from {targets} target(s); {path} not written"
UNREADABLE_CASE_MSG = (
    "no `test <name> ... <outcome>` line and no libtest summary in this target's test.log; "
    "Bazel reported {status}. The tail of the log follows"
)
STATUS_CASE_MSG = (
    "Bazel reported {status} for this target while every case in its test.log passed (a crash "
    "after the summary, a timeout in teardown). The tail of the log follows"
)


@dataclasses.dataclass(frozen=True)
class Counts:
    executed: int
    ignored: int
    filtered: int
    measured: int = 0

    @property
    def lines(self) -> int:
        """Per-case lines the summary implies; libtest prints none for `filtered`."""
        return self.executed + self.ignored + self.measured


@dataclasses.dataclass(frozen=True)
class Case:
    name: str
    outcome: str
    detail: str

    @property
    def failed(self) -> bool:
        return self.outcome.startswith("FAILED")

    @property
    def skipped(self) -> bool:
        return self.outcome.startswith("ignored")


@dataclasses.dataclass(frozen=True)
class Run:
    """One target's highest-numbered attempt. `log` is None when the event named none."""

    log: Path | None
    status: str
    seconds: float


@dataclasses.dataclass(frozen=True)
class Row:
    label: str
    crate: str
    suite: str
    cases: int
    skipped: int = 0
    ignored: int = 0


# ---------------------------------------------------------------------------
# Readers
# ---------------------------------------------------------------------------


def read_map(path: Path) -> tuple[list[Row], list[Finding]]:
    """An absent or unparseable map is a `map-unreadable` finding, never an empty
    map: `--update` would otherwise rewrite it from one run's targets alone. An
    `[[excluded]]` row is `map-excluded`: nothing runs a case Bazel skips."""
    try:
        payload = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        return [], [Finding("map-unreadable", MAP_UNREADABLE_MSG.format(path=path, error=error))]
    if payload.get("excluded"):
        return [], [Finding("map-excluded", MAP_EXCLUDED_MSG.format(path=path))]
    rows = [
        Row(
            label=str(entry["label"]),
            crate=str(entry.get("crate", "")),
            suite=str(entry.get("suite", "")),
            cases=int(entry.get("cases", 0)),
            skipped=int(entry.get("skipped", 0)),
            ignored=int(entry.get("ignored", 0)),
        )
        for entry in payload.get("target", [])
        if "label" in entry
    ]
    return rows, []


def _log_path(uri: str) -> Path:
    return Path(urllib.request.url2pathname(urllib.parse.urlparse(uri).path))


def _seconds(payload: dict) -> float:
    duration = payload.get("testAttemptDuration")
    if isinstance(duration, str) and duration.endswith("s"):
        try:
            return float(duration[:-1])
        except ValueError:
            return 0.0
    try:
        return float(payload.get("testAttemptDurationMillis", 0)) / 1000.0
    except (TypeError, ValueError):
        return 0.0


def read_runs(bep: Path) -> dict[str, Run]:
    """`{label: Run}` over every `TestResult`; the highest attempt wins (a flaky
    target emits one event per attempt, and summing them would inflate the floor)."""
    best: dict[str, tuple[int, Run]] = {}
    try:
        text = bep.read_text(encoding="utf-8")
    except OSError:
        return {}
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        identifier = event.get("id", {}).get("testResult")
        payload = event.get("testResult")
        if not isinstance(identifier, dict) or not isinstance(payload, dict):
            continue
        label = str(identifier.get("label", ""))
        if not label:
            continue
        attempt = int(identifier.get("attempt", 1) or 1)
        logs = [
            output["uri"]
            for output in payload.get("testActionOutput", [])
            if output.get("name") == "test.log" and str(output.get("uri", "")).startswith("file:")
        ]
        run = Run(
            log=_log_path(logs[-1]) if logs else None,
            status=str(payload.get("status", "")),
            seconds=_seconds(payload),
        )
        if attempt >= best.get(label, (0, None))[0]:
            best[label] = (attempt, run)
    return {label: run for label, (_, run) in sorted(best.items())}


def _read_log(path: Path | None) -> str:
    if path is None:
        return ""
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def read_cases(log: str) -> list[Case]:
    """libtest's per-case lines, each failure carrying its panic block. Case lines
    *inside* a failure block are a test's captured stdout, not cases."""
    panics: dict[str, list[str]] = {}
    blocks: list[tuple[int, int]] = []
    for match in FAILURE_BLOCK.finditer(log):
        panics.setdefault(match["name"], []).append(match["body"].strip())
        blocks.append((match.start(), match.end()))
    return [
        Case(name=match["name"], outcome=match["outcome"], detail="\n\n".join(panics.get(match["name"], ())))
        for match in CASE_LINE.finditer(log)
        if not any(start <= match.start() < end for start, end in blocks)
    ]


def count_results(runs: dict[str, Run]) -> tuple[dict[str, Counts], list[Finding]]:
    observed: dict[str, Counts] = {}
    findings: list[Finding] = []
    for label, run in runs.items():
        if run.log is None:
            findings.append(Finding("floor-no-output", NO_OUTPUT_MSG.format(label=label)))
            continue
        matches = list(RESULT_LINE.finditer(_read_log(run.log)))
        if not matches:
            findings.append(Finding("floor-unreadable", UNREADABLE_MSG.format(label=label, path=run.log)))
            continue
        observed[label] = Counts(
            executed=sum(int(m["passed"]) + int(m["failed"]) for m in matches),
            ignored=sum(int(m["ignored"]) for m in matches),
            filtered=sum(int(m["filtered"]) for m in matches),
            measured=sum(int(m["measured"]) for m in matches),
        )
    return observed, findings


# ---------------------------------------------------------------------------
# Floor
# ---------------------------------------------------------------------------


def floor_findings(observed: dict[str, Counts], rows: list[Row]) -> list[Finding]:
    findings: list[Finding] = []
    if not rows or len(observed) < len(rows):
        findings.append(Finding("floor-reader", READER_MSG.format(read=len(observed), rows=len(rows))))
    for row in rows:
        seen = observed.get(row.label)
        if seen is None:
            findings.append(Finding("floor-missing", MISSING_MSG.format(label=row.label)))
        elif seen.executed < row.cases:
            findings.append(
                Finding("floor-shrank", SHRANK_MSG.format(label=row.label, observed=seen.executed, recorded=row.cases))
            )
    return findings


def count_findings(runs: dict[str, Run], observed: dict[str, Counts]) -> tuple[dict[str, int], list[Finding]]:
    """Per target: per-case lines vs the summary. Returns the per-case counts too."""
    written: dict[str, int] = {}
    findings: list[Finding] = []
    for label, counts in observed.items():
        written[label] = len(read_cases(_read_log(runs[label].log)))
        if written[label] != counts.lines:
            findings.append(
                Finding("junit-count", COUNT_MSG.format(label=label, written=written[label], expected=counts.lines))
            )
    return written, findings


# ---------------------------------------------------------------------------
# --junit
# ---------------------------------------------------------------------------


def _xml_text(text: str) -> str:
    return XML_FORBIDDEN.sub("", text)


def _tail(log: str, lines: int = 40) -> str:
    return "\n".join(log.splitlines()[-lines:])


def junit_tree(runs: dict[str, Run], observed: dict[str, Counts]) -> ET.Element:
    """One `testsuite` per target, one `testcase` per libtest case. A target whose
    log nothing can read, or that Bazel failed while every case passed, still gets
    a failing `testcase` named after the target: a red never vanishes."""
    root = ET.Element("testsuites")
    for label, run in runs.items():
        log = _read_log(run.log)
        cases = read_cases(log)
        if label not in observed:
            detail = f"{UNREADABLE_CASE_MSG.format(status=run.status or 'no status')}\n\n{_tail(log)}"
            cases = [*cases, Case(name=label, outcome="FAILED", detail=detail)]
        if run.status not in PASSING_STATUS and not any(case.failed for case in cases):
            detail = f"{STATUS_CASE_MSG.format(status=run.status or 'no status')}\n\n{_tail(log)}"
            cases = [*cases, Case(name=label, outcome="FAILED", detail=detail)]
        suite = ET.SubElement(
            root,
            "testsuite",
            {
                "name": label,
                "tests": str(len(cases)),
                "failures": str(sum(1 for case in cases if case.failed)),
                "errors": "0",
                "skipped": str(sum(1 for case in cases if case.skipped)),
                "time": f"{run.seconds:.3f}",
            },
        )
        for case in cases:
            element = ET.SubElement(
                suite, "testcase", {"classname": _xml_text(label), "name": _xml_text(case.name), "time": "0"}
            )
            if case.failed:
                message = _xml_text(case.detail.splitlines()[0] if case.detail else "") or "FAILED"
                ET.SubElement(element, "failure", {"message": message}).text = _xml_text(case.detail)
            elif case.skipped:
                ET.SubElement(element, "skipped", {"message": _xml_text(case.outcome)})
    return root


def write_junit(path: Path, runs: dict[str, Run], observed: dict[str, Counts]) -> tuple[int, list[Finding]]:
    root = junit_tree(runs, observed)
    written = sum(len(suite) for suite in root)
    if not written:
        return 0, [Finding("junit-empty", JUNIT_EMPTY_MSG.format(targets=len(runs), path=path))]
    root.set("tests", str(written))
    root.set("failures", str(len(root.findall("./testsuite/testcase/failure"))))
    root.set("errors", "0")
    root.set("skipped", str(len(root.findall("./testsuite/testcase/skipped"))))
    root.set("time", f"{sum(run.seconds for run in runs.values()):.3f}")
    path.parent.mkdir(parents=True, exist_ok=True)
    tree = ET.ElementTree(root)
    ET.indent(tree, space="  ")
    tree.write(path, encoding="utf-8", xml_declaration=True)
    return written, []


# ---------------------------------------------------------------------------
# --update
# ---------------------------------------------------------------------------


def _toml_str(value: str) -> str:
    return json.dumps(value)  # a JSON string is a valid TOML basic string


def render_map(rows: list[Row]) -> str:
    out = [MAP_HEADER]
    for row in sorted(rows, key=lambda r: r.label):
        out += ["", "[[target]]", f"label = {_toml_str(row.label)}", f"crate = {_toml_str(row.crate)}"]
        out += [f"suite = {_toml_str(row.suite)}", f"cases = {row.cases}"]
        if row.skipped:
            out.append(f"skipped = {row.skipped}")
        if row.ignored:
            out.append(f"ignored = {row.ignored}")
    return "\n".join(out) + "\n"


def updated_rows(observed: dict[str, Counts], rows: list[Row]) -> tuple[list[Row], list[Finding]]:
    """Every observed target gets a row; counts rise only. A new label gets
    `crate`/`suite` derived from it (`//crates/<c>:<c>_test` -> `<c>`,
    `//:<name>` -> the root package's integration suite)."""
    by_label = {row.label: row for row in rows}
    findings = [
        Finding("floor-shrank", SHRANK_MSG.format(label=row.label, observed=observed[row.label].executed, recorded=row.cases))
        for row in rows
        if row.label in observed and observed[row.label].executed < row.cases
    ]
    out: list[Row] = []
    for label, counts in observed.items():
        old = by_label.get(label)
        if old is None:
            package, _, name = label.removeprefix("//").partition(":")
            crate = package.removeprefix("crates/") or "ocx_mirror"
            suite = crate if name == f"{crate}_test" else f"{crate}::{name}"
            old = Row(label=label, crate=crate, suite=suite, cases=0)
        out.append(
            dataclasses.replace(
                old,
                cases=max(old.cases, counts.executed),
                skipped=counts.filtered,
                ignored=counts.ignored,
            )
        )
    out += [row for row in rows if row.label not in observed]
    return out, findings


# ---------------------------------------------------------------------------
# --coverage
# ---------------------------------------------------------------------------


def read_listing(path: Path) -> dict[str, set[str]]:
    """`cargo nextest list --message-format json` -> `{binary_id: {case, ...}}`,
    only the cases the listing's filter matched."""
    document = json.loads(path.read_text(encoding="utf-8"))
    return {
        binary_id: {
            name
            for name, case in suite.get("testcases", {}).items()
            if case.get("filter-match", {}).get("status", "matches") == "matches"
        }
        for binary_id, suite in document.get("rust-suites", {}).items()
    }


def executed_cases(runs: dict[str, Run], rows: list[Row]) -> tuple[dict[str, set[str]], list[Finding]]:
    """`{suite: {case, ...}}` Bazel ran, via each row's `suite`."""
    suite_of = {row.label: row.suite for row in rows}
    out: dict[str, set[str]] = {}
    findings: list[Finding] = []
    for label, run in runs.items():
        suite = suite_of.get(label)
        if not suite:
            findings.append(Finding("coverage-unlisted", UNLISTED_MSG.format(label=label)))
            continue
        # An `#[ignore]`d case counts as seen: nextest lists it and skips it too.
        out.setdefault(suite, set()).update(case.name for case in read_cases(_read_log(run.log)))
    return out, findings


def coverage_findings(listing: dict[str, set[str]], executed: dict[str, set[str]]) -> list[Finding]:
    wanted = {(suite, name) for suite, names in listing.items() for name in names}
    ran = {(suite, name) for suite, names in executed.items() for name in names}
    gap = sorted(wanted - ran)
    if not gap:
        return []
    names = "\n  ".join(f"{suite} {name}" for suite, name in gap)
    return [
        Finding(
            "coverage-gap",
            f"bazel test coverage: {len(gap)} case(s) `cargo nextest list` names are not executed by Bazel:\n  {names}",
        )
    ]


# ---------------------------------------------------------------------------
# Entry points
# ---------------------------------------------------------------------------


def read_bep(bep: Path) -> tuple[dict[str, Run], list[Finding]]:
    if not bep.is_file() or bep.stat().st_size == 0:
        return {}, [Finding("floor-empty", EMPTY_MSG.format(path=bep))]
    runs = read_runs(bep)
    if not runs:
        return {}, [Finding("floor-empty", EMPTY_MSG.format(path=bep))]
    return runs, []


def run_check(bep: Path, map_path: Path, junit: Path | None = None, update: bool = False) -> int:
    runs, findings = read_bep(bep)
    if findings:
        return report(findings)
    rows, map_red = read_map(map_path)
    observed, findings = count_results(runs)
    findings = map_red + findings
    written, count_red = count_findings(runs, observed)
    findings += count_red
    if update:
        rows, shrank = updated_rows(observed, rows)
        findings += shrank
        if not findings:
            map_path.write_text(render_map(rows), encoding="utf-8")
            print(f"bazel test floor: {map_path} rewritten — {len(rows)} target row(s)")
    findings += floor_findings(observed, rows)
    findings += [
        Finding("floor-unlisted", UNLISTED_MSG.format(label=label))
        for label in observed
        if label not in {row.label for row in rows}
    ]
    for label, counts in observed.items():
        print(
            f"  {label:60} cases {written[label]:5}  libtest total {counts.lines:5}"
            f"  (filtered {counts.filtered})  {'==' if written[label] == counts.lines else '!='}"
        )
    total_lines = sum(counts.lines for counts in observed.values())
    if junit is not None:
        cases, junit_red = write_junit(junit, runs, observed)
        findings += junit_red
        if cases:
            print(
                f"bazel junit: {cases} <testcase> element(s) across {len(runs)} target(s) written to "
                f"{junit}; sum of libtest totals {total_lines}"
            )
    if not findings:
        print(
            f"bazel test floor: {len(observed)} target(s), {sum(c.executed for c in observed.values())} "
            f"executed, {sum(c.ignored for c in observed.values())} ignored, "
            f"{sum(c.filtered for c in observed.values())} filtered out — every one of {len(rows)} "
            "recorded targets at or above its count"
        )
    return report(findings)


def run_coverage(bep: Path, map_path: Path, listing_path: Path) -> int:
    runs, findings = read_bep(bep)
    if findings:
        return report(findings)
    rows, findings = read_map(map_path)
    if findings:
        return report(findings)
    listing = read_listing(listing_path)
    executed, findings = executed_cases(runs, rows)
    findings += coverage_findings(listing, executed)
    listed = sum(len(names) for names in listing.values())
    ran = sum(len(names) for names in executed.values())
    print(
        f"bazel test coverage: nextest lists {listed} case(s) in {len(listing)} suite(s); "
        f"Bazel executed {ran}"
    )
    return report(findings)


# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------


def _log(path: Path, *, passed: int, failed: int = 0, ignored: int = 0, filtered: int = 0,
         mangle: bool = False, extra_case_lines: int = 0) -> str:
    """A libtest log, written to `path`; returns its `file://` URI. `mangle` breaks
    the per-case separator while the summary still parses; `extra_case_lines`
    plants case lines the summary does not count."""
    sep = " .. " if mangle else " ... "
    lines = [f"running {passed + failed + ignored} tests"]
    lines += [f"test fixture::passes_{i}{sep}ok" for i in range(passed + extra_case_lines)]
    lines += [f"test fixture::skips_{i}{sep}ignored" for i in range(ignored)]
    lines += [f"test fixture::fails_{i}{sep}FAILED" for i in range(failed)]
    if failed:
        lines += ["", "failures:"]
        for i in range(failed):
            lines += [
                "",
                f"---- fixture::fails_{i} stdout ----",
                f"thread 'fixture::fails_{i}' panicked at crates/demo/src/lib.rs:7:5:",
                "assertion `left == right` failed",
                # A test printing a libtest-shaped line must not count as a case.
                "test fixture::printed ... ok",
            ]
        lines += ["", "failures:"] + [f"    fixture::fails_{i}" for i in range(failed)]
    lines += [
        "",
        f"test result: {'FAILED' if failed else 'ok'}. {passed} passed; {failed} failed; "
        f"{ignored} ignored; 0 measured; {filtered} filtered out; finished in 0.01s",
        "",
    ]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines), encoding="utf-8")
    return path.as_uri()


def _bep(path: Path, entries: dict[str, str | None], *, status: str = "PASSED", xml: bool = True) -> Path:
    """A BEP with one `TestResult` per entry (URI of its test.log, or None for an
    event naming no output at all) plus noise events a positional reader would trip on."""
    lines = [json.dumps({"id": {"started": {}}, "started": {"uuid": "fixture"}})]
    for label, uri in entries.items():
        outputs = []
        if uri is not None:
            if xml:
                outputs.append({"name": "test.xml", "uri": uri.replace("test.log", "test.xml")})
            outputs.append({"name": "test.log", "uri": uri})
        lines.append(
            json.dumps(
                {
                    "id": {"testResult": {"label": label, "run": 1, "shard": 1, "attempt": 1}},
                    "testResult": {"status": status, "testAttemptDuration": "0.051s", "cachedLocally": True,
                                   "testActionOutput": outputs},
                }
            )
        )
        lines.append(json.dumps({"id": {"targetCompleted": {"label": label}}}))
    lines.append(json.dumps({"id": {"buildFinished": {}}}))
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def _fixture(work: Path, rows: list[Row], name: str, *, mangled: frozenset[str] = frozenset()) -> Path:
    """The live map replayed as a stream that satisfies it exactly."""
    entries: dict[str, str | None] = {
        row.label: _log(
            work / name / row.label.replace("//", "").replace(":", "/").lstrip("/") / "test.log",
            passed=row.cases, ignored=row.ignored, filtered=row.skipped, mangle=row.label in mangled,
        )
        for row in rows
    }
    return _bep(work / f"{name}.json", entries)


def prove_floor(work: Path, rows: list[Row]) -> None:
    runs = read_runs(_fixture(work, rows, "green"))
    observed, red = count_results(runs)
    _, count_red = count_findings(runs, observed)
    findings = red + count_red + floor_findings(observed, rows)
    expect(not findings, f"the green fixture must produce no finding, got {codes(findings)}")
    print(f"FLOOR GREEN : {len(observed)} targets, {sum(c.executed for c in observed.values())} executed — no finding")

    dropped = dict(observed)
    del dropped[rows[0].label]
    expect("floor-missing" in codes(floor_findings(dropped, rows)), "a dropped target must red")
    print(f"FLOOR RED   : {rows[0].label} dropped -> floor-missing")

    biggest = max(rows, key=lambda row: row.cases)
    shrunk = dict(observed)
    shrunk[biggest.label] = dataclasses.replace(observed[biggest.label], executed=biggest.cases - 1)
    red = floor_findings(shrunk, rows)
    expect(codes(red) == ["floor-shrank"], f"one deleted test must red alone, got {codes(red)}")
    print(f"FLOOR RED   : {red[0].message[:110]}")

    partial = {row.label: observed[row.label] for row in rows[:1]}
    expect("floor-reader" in codes(floor_findings(partial, rows)), "a partial read must red")
    expect("floor-reader" in codes(floor_findings(observed, [])), "an empty map must red")
    print("FLOOR RED   : a partial read and an empty map -> floor-reader")

    mute = work / "mute" / "test.log"
    mute.parent.mkdir(parents=True)
    mute.write_text("running 5 tests\n<the harness died here>\n", encoding="utf-8")
    _, red = count_results(read_runs(_bep(work / "mute.json", {"//crates/x:y": mute.as_uri()})))
    expect(codes(red) == ["floor-unreadable"], f"a mute log must red, got {codes(red)}")
    print("FLOOR RED   : a log with no summary -> floor-unreadable")

    empty = work / "empty.json"
    empty.write_text("", encoding="utf-8")
    expect(run_check(empty, TEST_TARGET_MAP) == 1, "an empty BEP must exit 1")
    expect(run_check(work / "absent.json", TEST_TARGET_MAP) == 1, "an absent BEP must exit 1")
    print("FLOOR RED   : an empty and an absent build event stream both exit 1")


def prove_edge_cases(work: Path) -> None:
    """The plan's edge-case table rows for the floor."""
    # JUnit count != libtest summary: two planted case lines the summary does not count.
    label = "//crates/demo:demo_test"
    uri = _log(work / "mismatch" / "test.log", passed=3, extra_case_lines=2)
    runs = read_runs(_bep(work / "mismatch.json", {label: uri}))
    observed, _ = count_results(runs)
    written, red = count_findings(runs, observed)
    expect(codes(red) == ["junit-count"] and written[label] == 5, f"5 lines vs a total of 3 must red, got {codes(red)}")
    print(f"EDGE RED    : {red[0].message[:110]}")

    # A cached target whose test.xml was not downloaded: JUnit still rebuilt from test.log.
    uri = _log(work / "logonly" / "test.log", passed=4)
    bep = _bep(work / "logonly.json", {label: uri}, xml=False)
    runs = read_runs(bep)
    observed, red = count_results(runs)
    cases, junit_red = write_junit(work / "logonly.xml", runs, observed)
    expect(not red and not junit_red and cases == 4, f"a log-only target must yield 4 cases, got {cases}")
    expect(not (work / "logonly" / "test.xml").exists(), "the fixture must carry no test.xml")
    print("EDGE GREEN  : test.log present, test.xml absent -> JUnit rebuilt, 4 cases")

    # Neither file: the event names no output at all.
    _, red = count_results(read_runs(_bep(work / "neither.json", {label: None})))
    expect(codes(red) == ["floor-no-output"], f"no test.log and no test.xml must red, got {codes(red)}")
    print(f"EDGE RED    : {red[0].message[:110]}")


def prove_junit(work: Path, rows: list[Row]) -> None:
    bep = _fixture(work, rows, "junit")
    runs = read_runs(bep)
    observed, _ = count_results(runs)
    cases, red = write_junit(work / "reports" / "unit.xml", runs, observed)
    document = ET.parse(work / "reports" / "unit.xml")
    recorded = sum(row.cases + row.ignored for row in rows)
    expect(not red and len(document.findall(".//testcase")) == cases == recorded,
           f"{cases} testcases for {recorded} recorded cases")
    expect(len(document.findall("testsuite")) == len(rows), "every target owns a testsuite, empty or not")
    print(f"JUNIT GREEN : {cases} <testcase> across {len(rows)} targets — exactly the recorded counts")

    label = "//crates/demo:demo_test"
    runs = read_runs(_bep(work / "fail.json", {label: _log(work / "fail" / "test.log", passed=2, failed=1)},
                          status="FAILED"))
    observed, _ = count_results(runs)
    written, count_red = count_findings(runs, observed)
    cases, _ = write_junit(work / "reports" / "fail.xml", runs, observed)
    named = ET.parse(work / "reports" / "fail.xml").find(".//testcase[@name='fixture::fails_0']/failure")
    expect(named is not None and "assertion `left == right` failed" in (named.text or ""),
           "the failing case must appear under its own name with its panic text")
    expect(cases == 3 and not count_red, f"3 cases ran (a printed case line is not one), got {cases}/{written}")
    print("JUNIT GREEN : failing case named `fixture::fails_0`, panic attached, printed case line ignored")

    victim = max(rows, key=lambda row: row.cases)
    runs = read_runs(_fixture(work, rows, "mangled", mangled=frozenset({victim.label})))
    observed, _ = count_results(runs)
    _, red = count_findings(runs, observed)
    expect(codes(red) == ["junit-count"] and len(red) == 1, f"one mangled target must red alone, got {codes(red)}")
    print(f"JUNIT RED   : mangled separator in {victim.label} -> junit-count")

    dead = work / "dead" / "test.log"
    dead.parent.mkdir(parents=True)
    dead.write_text("running 5 tests\n<the harness died here>\n", encoding="utf-8")
    runs = read_runs(_bep(work / "dead.json", {"//crates/x:y_test": dead.as_uri()}, status="TIMEOUT"))
    observed, _ = count_results(runs)
    cases, _ = write_junit(work / "reports" / "dead.xml", runs, observed)
    fallback = ET.parse(work / "reports" / "dead.xml").find(".//testcase[@name='//crates/x:y_test']/failure")
    expect(cases == 1 and fallback is not None and "harness died" in (fallback.text or ""),
           "an unreadable log must still report its target with the log's tail")
    print("JUNIT RED   : unreadable log -> the target itself is a failing testcase")

    runs = read_runs(_bep(work / "crash.json", {"//crates/x:crash_test": _log(work / "crash" / "test.log", passed=3)},
                          status="FAILED"))
    observed, _ = count_results(runs)
    cases, _ = write_junit(work / "reports" / "crash.xml", runs, observed)
    expect(cases == 4, f"3 passing cases plus the target's own failure, got {cases}")
    print("JUNIT RED   : FAILED target whose cases all passed -> named anyway")

    _, red = write_junit(work / "reports" / "none.xml", {}, {})
    expect(codes(red) == ["junit-empty"] and not (work / "reports" / "none.xml").exists(), "no case must write no report")
    print("JUNIT RED   : nothing to report -> junit-empty, no file written")


def prove_update(work: Path, rows: list[Row]) -> None:
    runs = read_runs(_fixture(work, rows, "update"))
    observed, _ = count_results(runs)
    grown = dict(observed)
    victim = rows[0].label
    grown[victim] = dataclasses.replace(observed[victim], executed=observed[victim].executed + 2)
    grown["//crates/ocx_mirror_new:ocx_mirror_new_test"] = Counts(executed=5, ignored=0, filtered=0)
    new_rows, red = updated_rows(grown, rows)
    by_label = {row.label: row for row in new_rows}
    expect(not red and by_label[victim].cases == rows[0].cases + 2, "a rising count must be written")
    new = by_label["//crates/ocx_mirror_new:ocx_mirror_new_test"]
    expect(new.cases == 5 and new.suite == "ocx_mirror_new", f"a new target gets a row, got {new}")
    rendered = work / "map.toml"
    rendered.write_text(render_map(new_rows), encoding="utf-8")
    reread_rows, _ = read_map(rendered)
    expect(reread_rows == sorted(new_rows, key=lambda r: r.label), "the rendered map must round-trip")
    print(f"UPDATE GREEN: +2 on {victim} and one new target written; map round-trips")

    shrunk = dict(observed)
    shrunk[victim] = dataclasses.replace(observed[victim], executed=rows[0].cases - 1)
    _, red = updated_rows(shrunk, rows)
    expect(codes(red) == ["floor-shrank"], f"--update must refuse to lower a count, got {codes(red)}")
    print("UPDATE RED  : a falling count is refused, not written")

    broken = work / "broken.toml"
    text = render_map(rows).replace("\n[[target]]\n", "\n[[target]\n", 1)
    broken.write_text(text, encoding="utf-8")
    _, red = read_map(broken)
    expect(codes(red) == ["map-unreadable"] and str(broken) in red[0].message, f"a broken map must red, got {codes(red)}")
    bep = _fixture(work, rows, "broken-map")
    expect(run_check(bep, broken, update=True) == 1, "--update over a broken map must exit 1")
    expect(broken.read_text(encoding="utf-8") == text, "--update must not rewrite a map it could not read")
    print("UPDATE RED  : a syntax-broken map -> map-unreadable, exit 1, file untouched")


def prove_coverage(work: Path, rows: list[Row]) -> None:
    executed = {row.suite: {f"case_{i}" for i in range(3)} for row in rows}
    listing = {suite: set(names) for suite, names in executed.items()}
    expect(not coverage_findings(listing, executed), "listing == executed must be green")
    print("COVER GREEN : nextest list == Bazel executed")

    thin = {suite: set(names) for suite, names in executed.items()}
    thin[rows[0].suite].discard("case_0")
    red = coverage_findings(listing, thin)
    expect(codes(red) == ["coverage-gap"] and "case_0" in red[0].message, f"a dropped case must red by name, got {codes(red)}")
    print(f"COVER RED   : {red[0].message.splitlines()[0][:100]} ... case_0")

    excluded = work / "excluded.toml"
    excluded.write_text(render_map(rows) + '\n[[excluded]]\nsuite = "s"\nname = "n"\n', encoding="utf-8")
    _, red = read_map(excluded)
    expect(codes(red) == ["map-excluded"], f"an [[excluded]] row must red, got {codes(red)}")
    print("COVER RED   : a map carrying an [[excluded]] row -> map-excluded")


def self_test() -> int:
    rows, red = read_map(TEST_TARGET_MAP)
    expect(not red, f"{TEST_TARGET_MAP} must read: {codes(red)}")
    expect(rows, f"{TEST_TARGET_MAP} has no [[target]] rows — the fixture would be empty")
    scratch = REPO_ROOT / ".tmp"
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch) as directory:
        work = Path(directory)
        prove_floor(work, rows)
        prove_edge_cases(work)
        prove_junit(work, rows)
        prove_update(work, rows)
        prove_coverage(work, rows)
    print("bazel test floor self-test: every finding shown red, the live map shown green")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--bep", type=Path, help="the --build_event_json_file to judge")
    parser.add_argument("--map", type=Path, default=TEST_TARGET_MAP, help="the target map (red proofs)")
    parser.add_argument("--junit", type=Path, help="with --bep: write the per-case JUnit report here")
    parser.add_argument("--update", action="store_true", help="with --bep: rewrite [[target]] rows (rise only)")
    parser.add_argument("--coverage", type=Path, metavar="LISTING", help="with --bep: C4 coverage gate")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.coverage is not None:
        return run_coverage(args.bep, args.map, args.coverage)
    return run_check(args.bep, args.map, args.junit, args.update)


if __name__ == "__main__":
    raise SystemExit(main())
