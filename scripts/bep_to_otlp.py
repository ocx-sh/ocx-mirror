#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Bazel BEP -> OTLP traces: one `summary` span per invocation, one `target` span per `TargetComplete`.

    scripts/bep_to_otlp.py --self-test
    scripts/bep_to_otlp.py --bep bep.json [--min-targets 8]

Ported from ocx `scripts/bep_to_otlp.py` (ADR `adr_bazel_crate_split.md`
§ A-8, C-013) — see that copy for the design history: the library search, the
field allowlist's derivation, the cache-state measurements. Behaviour is
identical except:

* resource `service.name = ocx-mirror-bazel-build` (ocx: `bazel-build`) and a
  resource `vcs.repository.name = ocx-mirror`, so ocx's dashboards stay
  ocx-only by default;
* the `--min-targets` floor is the mirror-local `MIN_TARGETS` — **8**, the
  seven `crates/*` libraries plus the root library: a floor that tells "the
  reader read nothing" from "the build was clean", not a pin on the graph;
* `REPO_ROOT`, `Finding`, `codes`, `report` and `expect` are inlined (ocx's
  `bazel_gate_proofs` is not ported);
* fixtures live in `tests/fixtures/bep/`, and the persistence scan reads the
  mirror's `taskfiles/bazel.taskfile.yml` and `.github/workflows/verify.yml`.

Three rules the code below holds, each proved by `--self-test`:

* **The input is a credential dump.** Bazel writes the whole client
  environment and every rc flag value into the BEP (escaped: `=` is
  `\\u003d`, so a grep for `--client_env=` finds none of it). It must never
  persist: `bazel:build:nobuild` writes it to a 0700 `mktemp -d` outside the
  checkout that a `defer:` removes; `bep_persistence_findings` holds that.
* **`READ_FIELDS` is the whole read surface, per field** — `BuildStarted`
  carries the invocation id and, in `optionsDescription`, the expanded
  options, so an event-level allowlist would admit the secret.
* **Cache state is keyed on `executionInfo.strategy` alone.** On Bazel 9.2.0
  `cachedRemotely` is true for a plain `--disk_cache` hit and `cachedLocally`
  only for the in-memory cache; the four invocations in
  `tests/fixtures/bep/cache_states.json` (ported verbatim from ocx) show both
  answering wrongly.

Exits: **0, silent** with `OTEL_EXPORTER_OTLP_ENDPOINT` unset; **0** when
every target span was accepted (a failed build still exits 0 — telemetry,
not a verdict); **1** on any floor (empty BEP, an invocation with no
`started.uuid` or no `TargetComplete`, fewer than `--min-targets` targets),
any rejected span, any transport failure — loud on stderr.

`--self-test` runs under `task scripts:self-test`; the export under
`task telemetry:bazel BEP=<file>`, at the tail of `bazel:build:nobuild`.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent

# ponytail: the reader floor, mirror-local. 7 `crates/*` libraries + the root
# library; see the module docstring for why it is this low.
MIN_TARGETS = 8


@dataclasses.dataclass(frozen=True)
class Finding:
    """One exit-1 reason. `code` is what tests assert on; `message` is stderr.

    Separate on purpose: asserting a substring of an English sentence is one of
    the cheapest ways to write an assertion that also matches the opposite
    outcome.
    """

    code: str
    message: str


def codes(findings: list[Finding]) -> list[str]:
    return sorted({finding.code for finding in findings})


def report(findings: list[Finding]) -> int:
    """Exit 0 and silent, or exit 1 with one line per finding."""
    # stdout is block-buffered when piped and stderr is not, so without this
    # the findings land above the self-test lines that introduce them.
    sys.stdout.flush()
    for finding in findings:
        print(finding.message, file=sys.stderr)
    return 1 if findings else 0


def expect(condition: bool, problem: str) -> None:
    """A loud exit — a bare `assert` vanishes under `python3 -O`."""
    if not condition:
        raise SystemExit(f"bep_to_otlp self-test: {problem}")


# ---------------------------------------------------------------------------
# The span schema. Pinned by a sibling repository, not by this file.
# ---------------------------------------------------------------------------

#: `monitoring/grafana/dashboards/bazel-build.json` (server-hetzner1) selects
#: on `resource.service.name=~"${repo:regex}"`, whose `ocx-mirror` value is
#: this string (ADR A-8). Changing it blanks the mirror's series — to *no
#: data*, never to a false zero divergence.
SERVICE_NAME = "ocx-mirror-bazel-build"

#: Resource `vcs.repository.name`. Descriptive only — no dashboard query reads
#: it (A-8) — but it is the one attribute that names the repo outright.
REPOSITORY_NAME = "ocx-mirror"

#: `span.bazel.kind` — the discriminator both Tempo panels select on.
KIND_SUMMARY = "summary"
KIND_TARGET = "target"

#: OTLP enum values, sent numerically as the protobuf JSON mapping requires.
SPAN_KIND_INTERNAL = 1
STATUS_OK = 1
STATUS_ERROR = 2

#: The read surface, field by field. Everything else in the BEP — including
#: `structuredCommandLine`, `unstructuredCommandLine`, `optionsParsed`,
#: `workspaceStatus`, `buildMetadata` and `started.optionsDescription` — is
#: never read, and `prove_no_secret` asserts that against a planted credential.
READ_FIELDS: dict[str, tuple[str, ...]] = {
    "started": ("uuid", "startTimeMillis"),
    "completed": ("success",),
    "testResult": (
        "status",
        "testAttemptDurationMillis",
        "testAttemptStartMillisEpoch",
        "executionInfo.strategy",
    ),
    "finished": ("overallSuccess", "finishTimeMillis"),
}

#: `executionInfo.strategy` on a cache hit is the runner's name, and the runner
#: names the tier. Both spellings are Bazel's, not this file's — they are what
#: `BuildMetrics.actionSummary.runnerCount` calls the same spawns.
RUNNER_REMOTE_CACHE = "remote cache hit"
RUNNER_DISK_CACHE = "disk cache hit"

#: `span.bazel.cache`. Four values, exhaustive over the measured matrix.
CACHE_REMOTE = "remote"
CACHE_DISK = "disk"
#: No `executionInfo.strategy` on a `TestResult` — measured, the submessage is
#: present and empty: the in-memory action cache of a bazel server that already
#: ran this action. Never seen on a fresh server, so never seen on CI.
CACHE_ACTION = "action"
CACHE_EXECUTED = "executed"

FLOOR_EMPTY_MSG = (
    "bep_to_otlp read 0 events from {path} — the BEP is absent or empty, and a reader "
    "that parsed nothing is indistinguishable from a clean build"
)
FLOOR_NO_INVOCATION_MSG = (
    "bep_to_otlp read {events} event(s) from {path} but found no BuildStarted.uuid — "
    "the reader stopped early, or the file is not a build event JSON stream"
)
FLOOR_NO_TARGETS_MSG = (
    "bep_to_otlp read invocation {build_id} with 0 TargetComplete events — "
    "the reader stopped early"
)
FLOOR_MIN_TARGETS_MSG = (
    "bep_to_otlp read {targets} target(s) across {invocations} invocation(s), expected "
    ">= {floor} for this invocation's pattern — the reader stopped early"
)
DROP_MSG = (
    "bep_to_otlp exported {exported} spans for {targets} targets — the export dropped spans"
)
TRANSPORT_MSG = "bep_to_otlp could not reach {url}: {detail}"
REFUSED_FIELD_MSG = "bep_to_otlp: refused field {name} — not in the read allowlist"


# ---------------------------------------------------------------------------
# Where the stream is allowed to live. See the docstring section above.
# ---------------------------------------------------------------------------

#: The writer's flag, this reader's own argument, and the go-task variable the
#: two resolve through. The variable is in here because that is where a path
#: moves: `--build_event_json_file={{.BEP}}` does not change when `BEP` is
#: re-pointed at `target/`, so a reader looking only at argv sees nothing.
BEP_ARG = re.compile(
    r"--build_event_json_file=(\S+)|--bep[= ]+(\S+)|^\s+BEP:\s*(\S+)", re.MULTILINE
)

#: The one sanctioned scratch spelling in `taskfiles/bazel.taskfile.yml`, and
#: the `defer:` that removes it. Named rather than pattern-matched: there is
#: one producer in this repository, and a finding that cannot name the variable
#: it wants is a finding nobody can act on.
SCRATCH_VAR = "BEP_DIR"
SCRATCH_DECL = re.compile(rf"{SCRATCH_VAR}:\s*\n\s*sh:\s*mktemp -d")
SCRATCH_DEFER = re.compile(rf"defer:\s*rm -rf\s*'?\{{\{{\.{SCRATCH_VAR}\}}\}}")

BEP_IN_WORKSPACE_MSG = (
    "{source} writes or reads a build event stream at {path!r}, which resolves inside the "
    "checkout. Bazel serialises the whole client environment into that file — measured on "
    "this tree, 276 `--client_env` entries including AWS_SECRET_ACCESS_KEY and every "
    "`--remote_header=authorization=Basic …` — and `Swatinem/rust-cache` saves `./target` "
    "on a push to main. Write it under a `mktemp -d` outside the workspace"
)
BEP_NOT_DELETED_MSG = (
    "{source} names a build event stream but no `{var}: sh: mktemp -d` and `defer: rm -rf "
    "{{{{.{var}}}}}` pair removes it. An unlink placed after the build is a cleanup a failed "
    "step skips; go-task runs a `defer:` item on every exit"
)


def bep_persistence_findings(sources: dict[str, str]) -> list[Finding]:
    """Every build-event-stream path in `sources`, and whether it survives.

    Textual, over the recipe rather than over a run, for the reason the rest of
    this file's floors are textual: the alternative is a 12-second `bazel`
    invocation inside `task scripts:self-test`. What it therefore asserts is the
    *spelling* — a path that is not rooted at the scratch variable, or a
    scratch variable no `defer:` removes.
    """
    findings: list[Finding] = []
    for source, text in sorted(sources.items()):
        paths = [next(filter(None, groups)).strip("'\"") for groups in BEP_ARG.findall(text)]
        for path in paths:
            # Inside the checkout is either an explicit `{{.ROOT_DIR}}` or a
            # relative path, which go-task and a `run:` body both resolve
            # against the working directory — the checkout in both lanes.
            if "ROOT_DIR" in path or not path.startswith(("{{.", "/", "$")):
                findings.append(
                    Finding(
                        "bep-in-workspace",
                        BEP_IN_WORKSPACE_MSG.format(source=source, path=path),
                    )
                )
        if paths and not (SCRATCH_DECL.search(text) and SCRATCH_DEFER.search(text)):
            findings.append(
                Finding(
                    "bep-not-deleted",
                    BEP_NOT_DELETED_MSG.format(source=source, var=SCRATCH_VAR),
                )
            )
    return findings


# ---------------------------------------------------------------------------
# Reading. The BEP is JSON; stdlib owns the codec, this owns the field choice.
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Target:
    """One `TargetComplete`, plus its `TestResult` timing and cache tier."""

    label: str
    success: bool
    start_millis: int | None = None
    duration_millis: int | None = None
    #: One of the `CACHE_*` tiers, or `""` for a target with no `TestResult` —
    #: a `rust_library` has no spawn of its own in the BEP, and inventing a
    #: tier for it would put a made-up value on the dashboard.
    cache: str = ""


@dataclasses.dataclass
class Invocation:
    """One Bazel invocation's readable surface. Nothing else is retained."""

    build_id: str
    start_millis: int
    finish_millis: int | None = None
    overall_success: bool = False
    targets: dict[str, Target] = dataclasses.field(default_factory=dict)

    @property
    def target_count(self) -> int:
        return len(self.targets)


def _millis(value: Any) -> int | None:
    """proto3 JSON renders int64 as a *string*; a missing zero is simply absent."""
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _truthy(payload: dict[str, Any], field: str) -> bool:
    """`payload[field] is True`, and never `.get(field) is not False`.

    proto3 JSON **omits a false boolean**. Measured on a real failing build:
    `//:boom`'s `completed` payload carries no `success` key at all, and
    `BuildFinished.finished` carries no `overallSuccess`. The natural spelling
    `.get("success") is not False` returns `True` for both — it reports a
    failed build as green. `_presence_reader` below keeps that inversion as a
    named control.
    """
    return payload.get(field) is True


def _cache_tier(payload: dict[str, Any]) -> str:
    """Which cache served this `TestResult`, read from the runner that served it.

    The only field consulted is `executionInfo.strategy`. See the module
    docstring's table for why the two boolean fields next to it cannot carry
    this question: one is true for a disk hit with no remote cache configured,
    the other is false for every disk hit.

    An unrecognised strategy is `executed`, never a cache tier. Bazel's runner
    vocabulary is open (`linux-sandbox`, `local`, `worker`, `processwrapper-
    sandbox`, `remote`, and whatever a future release adds), and the safe
    default for a value this reader has never seen is "it ran" — over-reporting
    cache hits is the failure this whole reader exists to avoid.

    **No strategy at all is the action cache, and it does not mean the field is
    missing.** Measured: on a re-run against a live bazel server the payload
    carries `"executionInfo": {}` — the submessage is present and *empty*,
    because proto3 JSON emits it while omitting every default field inside it.
    A reader spelled `"executionInfo" not in payload` therefore never sees that
    state at all and files it under `executed`, which is the opposite of the
    truth: nothing ran.
    """
    info = payload.get("executionInfo")
    strategy = info.get("strategy") if isinstance(info, dict) else None
    if not strategy:
        return CACHE_ACTION
    if strategy == RUNNER_REMOTE_CACHE:
        return CACHE_REMOTE
    if strategy == RUNNER_DISK_CACHE:
        return CACHE_DISK
    return CACHE_EXECUTED


def read_bep(path: Path) -> tuple[list[Invocation], list[Finding]]:
    """Every invocation in `path`, tolerating more than one stream in one file.

    Measured: a `--disk_cache` pointed at a fresh directory over a warm action
    cache produced `Lost inputs no longer available remotely`, a transparent
    Bazel retry, and **two invocation ids appended to one
    `--build_event_json_file`**. A reader keyed on "the" invocation silently
    reads half of such a file, so `started` opens a new invocation rather than
    overwriting the current one.
    """
    findings: list[Finding] = []
    if not path.is_file():
        return [], [Finding("bep-floor-empty", FLOOR_EMPTY_MSG.format(path=path))]

    invocations: list[Invocation] = []
    by_id: dict[str, Invocation] = {}
    current: Invocation | None = None
    events = 0

    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError as error:
            findings.append(
                Finding(
                    "bep-unreadable",
                    f"bep_to_otlp could not parse {path}:{number}: {error.msg} — "
                    "a half-read BEP would under-count targets silently",
                )
            )
            continue
        events += 1
        if not isinstance(event, dict):
            continue
        identity = event.get("id")
        identity = identity if isinstance(identity, dict) else {}

        if "started" in identity:
            payload = event.get("started") or {}
            build_id = str(payload.get("uuid") or "")
            if not build_id:
                continue
            current = Invocation(
                build_id=build_id,
                start_millis=_millis(payload.get("startTimeMillis")) or 0,
            )
            invocations.append(current)
            by_id[build_id] = current
            continue

        if current is None:
            # Events before the first `started` belong to no invocation. Kept
            # counted in `events` so the "no BuildStarted" floor can tell a
            # read-nothing run from a read-something-unexpected one.
            continue

        if "targetCompleted" in identity:
            label = str(identity["targetCompleted"].get("label") or "")
            if not label:
                continue
            payload = event.get("completed") or {}
            existing = current.targets.get(label)
            current.targets[label] = Target(
                label=label,
                success=_truthy(payload, "success"),
                start_millis=existing.start_millis if existing else None,
                duration_millis=existing.duration_millis if existing else None,
                cache=existing.cache if existing else "",
            )
        elif "testResult" in identity:
            label = str(identity["testResult"].get("label") or "")
            if not label:
                continue
            payload = event.get("testResult") or {}
            existing = current.targets.get(label)
            current.targets[label] = Target(
                label=label,
                # A target span reports whether the *target* completed, which
                # is not the test's verdict: measured, `//:fail_test` carries
                # `completed.success: true` while its `testResult.status` is
                # FAILED, because building the test succeeded.
                success=existing.success if existing else False,
                start_millis=_millis(payload.get("testAttemptStartMillisEpoch")),
                duration_millis=_millis(payload.get("testAttemptDurationMillis")),
                cache=_cache_tier(payload),
            )
        elif "buildFinished" in identity:
            payload = event.get("finished") or {}
            current.finish_millis = _millis(payload.get("finishTimeMillis"))
            current.overall_success = _truthy(payload, "overallSuccess")

    if events == 0:
        findings.append(Finding("bep-floor-empty", FLOOR_EMPTY_MSG.format(path=path)))
        return [], findings
    if not invocations:
        findings.append(
            Finding(
                "bep-floor-no-invocation",
                FLOOR_NO_INVOCATION_MSG.format(events=events, path=path),
            )
        )
    for invocation in invocations:
        if invocation.target_count == 0:
            findings.append(
                Finding(
                    "bep-floor-no-targets",
                    FLOOR_NO_TARGETS_MSG.format(build_id=invocation.build_id),
                )
            )
    return invocations, findings


def floor_findings(invocations: list[Invocation], minimum: int) -> list[Finding]:
    """The pattern floor, kept apart from the read so it can be shown red alone."""
    targets = sum(invocation.target_count for invocation in invocations)
    if targets < minimum:
        return [
            Finding(
                "bep-floor-min-targets",
                FLOOR_MIN_TARGETS_MSG.format(
                    targets=targets, invocations=len(invocations), floor=minimum
                ),
            )
        ]
    return []


# ---------------------------------------------------------------------------
# Span building. The schema half — every key here is read by a WP-27 panel.
# ---------------------------------------------------------------------------


def _attr(key: str, value: str | bool | int) -> dict[str, Any]:
    """One OTLP KeyValue. int64 is a *string* in the protobuf JSON mapping."""
    if isinstance(value, bool):
        return {"key": key, "value": {"boolValue": value}}
    if isinstance(value, int):
        return {"key": key, "value": {"intValue": str(value)}}
    return {"key": key, "value": {"stringValue": value}}


def _trace_id(build_id: str) -> str:
    """32 lowercase hex, so one build is one trace.

    A Bazel invocation id is a UUID, whose hex is exactly 32 characters — but
    BEP carries no published stability guarantee, so the shape is *checked*
    rather than assumed, and anything else is hashed to the right width.
    """
    bare = build_id.replace("-", "").lower()
    if len(bare) == 32 and all(character in "0123456789abcdef" for character in bare):
        return bare
    return hashlib.sha256(build_id.encode("utf-8")).hexdigest()[:32]


def _span_id(build_id: str, suffix: str) -> str:
    """16 lowercase hex, derived rather than random so the self-test is exact."""
    digest = hashlib.sha256(f"{build_id}\x00{suffix}".encode()).hexdigest()
    return digest[:16]


def _nanos(millis: int) -> str:
    return str(int(millis) * 1_000_000)


def spans_for(invocation: Invocation) -> list[dict[str, Any]]:
    """The summary span, then one target span per `TargetComplete`.

    `bazel.target_count` is the count this reader **read**, stamped on the
    summary span before a single target span is exported. That independence is
    the whole of S-013: the declared line comes from the BEP, the landed line
    from what reached Tempo, and a gap between them is the dropped-span class.
    """
    trace_id = _trace_id(invocation.build_id)
    root_id = _span_id(invocation.build_id, "summary")
    start = invocation.start_millis
    finish = invocation.finish_millis if invocation.finish_millis is not None else start

    summary = {
        "traceId": trace_id,
        "spanId": root_id,
        "name": "bazel.build",
        "kind": SPAN_KIND_INTERNAL,
        "startTimeUnixNano": _nanos(start),
        "endTimeUnixNano": _nanos(finish),
        "attributes": [
            _attr("bazel.kind", KIND_SUMMARY),
            _attr("bazel.build_id", invocation.build_id),
            _attr("bazel.target_count", invocation.target_count),
            _attr("bazel.success", invocation.overall_success),
        ],
        "status": {"code": STATUS_OK if invocation.overall_success else STATUS_ERROR},
    }

    spans = [summary]
    for label in sorted(invocation.targets):
        target = invocation.targets[label]
        # `TargetComplete` carries no timing field — measured, it has `success`,
        # `outputGroup`, `tag` and `testTimeout` and nothing else. A test target
        # gets its real window from `TestResult`; a `rust_library` has none in
        # the BEP at all, so its span is a zero-width marker at the build's
        # start rather than an invented duration.
        target_start = target.start_millis if target.start_millis is not None else start
        duration = target.duration_millis or 0
        attributes = [
            _attr("bazel.kind", KIND_TARGET),
            _attr("bazel.build_id", invocation.build_id),
            _attr("bazel.target", label),
            _attr("bazel.success", target.success),
        ]
        if target.duration_millis is not None:
            attributes.append(_attr("bazel.duration_ms", target.duration_millis))
        if target.cache:
            attributes.append(_attr("bazel.cache", target.cache))
        spans.append(
            {
                "traceId": trace_id,
                "spanId": _span_id(invocation.build_id, label),
                "parentSpanId": root_id,
                "name": label,
                "kind": SPAN_KIND_INTERNAL,
                "startTimeUnixNano": _nanos(target_start),
                "endTimeUnixNano": _nanos(target_start + duration),
                "attributes": attributes,
                "status": {"code": STATUS_OK if target.success else STATUS_ERROR},
            }
        )
    return spans


def resource_attributes(environ: Mapping[str, str]) -> list[dict[str, Any]]:
    """`service.name`, plus where the build ran.

    Without a provenance attribute every BEP span carries one identical
    resource, so a runner's build and a workstation's sit in the same series
    and the dashboard cannot tell a CI regression from someone's local
    experiment. `ocx.source` reuses the `ci`/`local` vocabulary
    `.github/actions/test-telemetry/action.yml` already pushes for the JUnit
    suites, so one dashboard variable filters both signals.

    The GitHub variables are ambient and public — a run id, a repository and a
    server URL. Nothing read here is a credential, which is the whole reason
    the allowlist is spelled out rather than the environment forwarded.
    """
    attributes = [
        _attr("service.name", SERVICE_NAME),
        _attr("vcs.repository.name", REPOSITORY_NAME),
    ]
    run_id = environ.get("GITHUB_RUN_ID", "").strip()
    repository = environ.get("GITHUB_REPOSITORY", "").strip()
    if not (run_id and repository):
        return attributes + [_attr("ocx.source", "local")]
    server = environ.get("GITHUB_SERVER_URL", "").strip().rstrip("/") or "https://github.com"
    return attributes + [
        _attr("ocx.source", "ci"),
        _attr("ci.run_url", f"{server}/{repository}/actions/runs/{run_id}"),
    ]


def otlp_document(
    spans: list[dict[str, Any]], environ: Mapping[str, str] | None = None
) -> dict[str, Any]:
    """An OTLP/HTTP `ExportTraceServiceRequest`, ready for `json.dumps`.

    `traceId` and `parentSpanId` are hex strings, not base64: OTLP/JSON
    deliberately departs from the protobuf JSON mapping for those two byte
    fields, and getting it wrong is the kind of thing a local fixture cannot
    see. `prove_live_push` is what settles it against a real collector.

    `environ` is explicit so a proof states which side of `resource_attributes`
    it is on; the callers that export take the process environment.
    """
    return {
        "resourceSpans": [
            {
                "resource": {
                    "attributes": resource_attributes(
                        os.environ if environ is None else environ
                    )
                },
                "scopeSpans": [{"scope": {"name": "bep_to_otlp"}, "spans": spans}],
            }
        ]
    }


# ---------------------------------------------------------------------------
# Export. stdlib owns the transport; this owns the drop accounting.
# ---------------------------------------------------------------------------

#: `(url, headers, body) -> (status, response_bytes)`.
Poster = Callable[[str, dict[str, str], bytes], "tuple[int, bytes]"]


def traces_url(environ: dict[str, str]) -> str | None:
    """The OTLP endpoint resolution the spec defines, and nothing beyond it."""
    signal = environ.get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "").strip()
    if signal:
        return signal
    base = environ.get("OTEL_EXPORTER_OTLP_ENDPOINT", "").strip()
    if not base:
        return None
    return f"{base.rstrip('/')}/v1/traces"


def otlp_headers(environ: dict[str, str]) -> dict[str, str]:
    """`OTEL_EXPORTER_OTLP_HEADERS=k=v,k=v`, split on the *first* `=` per pair.

    The deployed value is `Authorization=Basic <base64>`: base64 carries `=`
    padding and never a comma, so first-`=` splitting is what keeps the padding
    attached to the value. Left literal — the space in `Basic <…>` is a valid
    header value and is what junit2otlp already sends to this endpoint.
    """
    headers = {"Content-Type": "application/json"}
    raw = environ.get("OTEL_EXPORTER_OTLP_HEADERS", "")
    for pair in raw.split(","):
        if "=" not in pair:
            continue
        key, value = pair.split("=", 1)
        key = key.strip()
        if key:
            headers[key] = value.strip()
    return headers


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuse every 3xx instead of replaying the request at the new host.

    `urlopen`'s default `HTTPRedirectHandler` rebuilds the request for a
    redirect and strips only the *content* headers — `Authorization` survives,
    including across origins. `headers` here carries
    `Authorization: Basic <base64>` from `OTEL_OTLP_AUTH`, so a compromised or
    hijacked endpoint answering `302` would be handed the collector credential
    (CWE-200). OTLP/HTTP defines no redirect, so refusing one loses nothing that
    the specification offers.

    Returning `None` from `redirect_request` is urllib's documented "do not
    follow": the 3xx then surfaces as an `HTTPError`, which `export` already
    files as `otlp-transport`.
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Untyped on purpose: the signature is urllib's, and annotating it
        # here would pin six parameter types the standard library does not.
        return None


#: Built once. `urlopen` installs the default opener — with the redirect handler
#: — so the request has to go through an opener of our own to avoid it.
_OPENER = urllib.request.build_opener(_NoRedirect)


def post_otlp(url: str, headers: dict[str, str], body: bytes) -> tuple[int, bytes]:
    """One synchronous POST, following no redirect. No queue to overflow."""
    request = urllib.request.Request(url, data=body, headers=headers, method="POST")
    with _OPENER.open(request, timeout=30) as response:
        return response.status, response.read()


def rejected_spans(payload: bytes) -> int:
    """`partialSuccess.rejectedSpans` — the one drop vector HTTP leaves open.

    An empty body, or a body with no `partialSuccess`, means everything was
    accepted; that is what the collector returns on a clean export.
    """
    if not payload.strip():
        return 0
    try:
        document = json.loads(payload)
    except json.JSONDecodeError:
        return 0
    partial = document.get("partialSuccess") if isinstance(document, dict) else None
    if not isinstance(partial, dict):
        return 0
    return _millis(partial.get("rejectedSpans")) or 0


def export(
    spans: list[dict[str, Any]],
    url: str,
    headers: dict[str, str],
    batch: int,
    post: Poster = post_otlp,
) -> tuple[int, list[Finding]]:
    """Send every span; return how many the collector accepted."""
    accepted = 0
    findings: list[Finding] = []
    for index in range(0, len(spans), batch):
        chunk = spans[index : index + batch]
        body = json.dumps(otlp_document(chunk)).encode("utf-8")
        try:
            status, payload = post(url, headers, body)
        except (urllib.error.URLError, OSError) as error:
            findings.append(
                Finding("otlp-transport", TRANSPORT_MSG.format(url=url, detail=error))
            )
            continue
        if status < 200 or status >= 300:
            detail = payload.decode("utf-8", "replace")[:200]
            findings.append(
                Finding(
                    "otlp-transport",
                    TRANSPORT_MSG.format(url=url, detail=f"HTTP {status}: {detail}"),
                )
            )
            continue
        accepted += len(chunk) - rejected_spans(payload)
    return accepted, findings


def run(bep: Path, minimum: int, batch: int, environ: dict[str, str], post: Poster) -> int:
    """The whole pipeline, with its three loud exits and its one silent one."""
    url = traces_url(environ)
    if url is None:
        # The only sanctioned silent exit, matching telemetry.taskfile.yml:50-51.
        return 0

    invocations, findings = read_bep(bep)
    findings.extend(floor_findings(invocations, minimum))
    if findings:
        return report(findings)

    spans: list[dict[str, Any]] = []
    for invocation in invocations:
        spans.extend(spans_for(invocation))
    targets = sum(invocation.target_count for invocation in invocations)

    accepted, findings = export(spans, url, otlp_headers(environ), batch, post)
    if accepted != len(spans):
        findings.append(
            Finding("otlp-dropped", DROP_MSG.format(exported=accepted, targets=targets))
        )
    if findings:
        return report(findings)

    print(
        f"bep_to_otlp exported {accepted} spans for {targets} targets across "
        f"{len(invocations)} invocation(s), floor {minimum} — "
        f"service.name={SERVICE_NAME}, build_id(s) "
        f"{', '.join(invocation.build_id for invocation in invocations)}"
    )
    return 0


# ---------------------------------------------------------------------------
# Self-test.
# ---------------------------------------------------------------------------

FIXTURES = REPO_ROOT / "tests" / "fixtures" / "bep"

#: Planted into every committed fixture's `BuildStarted.optionsDescription`,
#: `structuredCommandLine`, `unstructuredCommandLine` and `optionsParsed`. If
#: this string reaches a span, the allowlist is not doing its job.
PLANTED_SECRET = "--remote_header=Authorization=Basic Y2k6c3VwZXJzZWNyZXQ="


def _presence_reader(path: Path) -> set[str]:
    """The wrong reader, kept as a named control, used nowhere else.

    `payload.get("success") is not False` is the natural spelling and it is
    inverted: proto3 JSON omits a false boolean, so a target that *failed*
    carries no key, `.get` returns `None`, and `None is not False` is `True`.
    It reports a failed build as entirely successful.
    """
    green: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        event = json.loads(line)
        label = event.get("id", {}).get("targetCompleted", {}).get("label")
        payload = event.get("completed")
        if payload is None or not label:
            continue
        if payload.get("success") is not False:
            green.add(label)
    return green


def _wide_reader(path: Path) -> list[dict[str, Any]]:
    """The other wrong reader: an *event-level* allowlist over `started`.

    Admitting the `started` event wholesale is the obvious way to reach
    `uuid` — and it carries `optionsDescription`, the fully expanded option
    string, straight into the span. Used nowhere else; shown leaking below.
    """
    attributes: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        event = json.loads(line)
        if "started" not in event.get("id", {}):
            continue
        for key, value in (event.get("started") or {}).items():
            if isinstance(value, str):
                attributes.append(_attr(f"bazel.{key}", value))
    return attributes


def _cached_flag_reader(path: Path) -> dict[str, str]:
    """The third wrong reader: cache state off the two boolean fields.

    `cachedLocally` and `executionInfo.cachedRemotely` are the fields whose
    names answer the question, so this is the spelling anyone writes first —
    the plan's own S-016 is phrased in its vocabulary. Measured, it is wrong in
    both directions at once, and `prove_cache_state` shows both:

    * on a `--disk_cache` hit with `--remote_cache=` empty it answers
      `remote` — a lane with no remote cache at all reads as remote-cached;
    * on that same run it answers nothing at all for `cachedLocally`, so a
      reader using only that half reports 0 of N cached where N of N were hits.

    Used nowhere else. One `{label: verdict}` per invocation, split on
    `started` exactly as `read_bep` splits, so the two are comparable
    invocation by invocation and a repeated label cannot collapse two runs
    into one.
    """
    per_invocation: list[dict[str, str]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        event = json.loads(line)
        identity = event.get("id", {})
        if "started" in identity:
            per_invocation.append({})
            continue
        label = identity.get("testResult", {}).get("label")
        payload = event.get("testResult")
        if payload is None or not label or not per_invocation:
            continue
        info = payload.get("executionInfo") or {}
        if info.get("cachedRemotely") is True:
            per_invocation[-1][label] = CACHE_REMOTE
        elif payload.get("cachedLocally") is True:
            per_invocation[-1][label] = CACHE_ACTION
        else:
            per_invocation[-1][label] = CACHE_EXECUTED
    return per_invocation


def recording_poster(
    status: int = 200, reject: int = 0, sink: list[bytes] | None = None
) -> Poster:
    """A `Poster` that records bodies and can claim a partial success."""

    def post(url: str, headers: dict[str, str], body: bytes) -> tuple[int, bytes]:
        del url, headers
        if sink is not None:
            sink.append(body)
        if reject:
            return status, json.dumps({"partialSuccess": {"rejectedSpans": str(reject)}}).encode()
        return status, b"{}"

    return post


def prove_failed_build() -> int:
    """A failing build must parse, and must parse as *failing*."""
    checks = 0
    path = FIXTURES / "failed_build.json"
    invocations, findings = read_bep(path)
    expect(findings == [], f"the committed failing-build fixture must read clean, got {findings}")
    expect(len(invocations) == 1, f"expected one invocation, got {len(invocations)}")
    invocation = invocations[0]
    expect(invocation.target_count == 4, f"expected 4 targets, got {invocation.target_count}")
    expect(not invocation.overall_success, "a failed build must not read as successful")
    failed = sorted(label for label, t in invocation.targets.items() if not t.success)
    expect(failed == ["//:boom"], f"exactly //:boom must read as failed, got {failed}")
    print(
        "S-013 GREEN: a real failing build parses — 4 targets, //:boom failed, "
        "overallSuccess absent and read as False"
    )
    checks += 1

    # The mutation is the reader, not the fixture: the same bytes, read the
    # natural-but-inverted way, must disagree.
    green = _presence_reader(path)
    expect(
        "//:boom" in green,
        "the control reader must mis-report //:boom as green, or it is not the control",
    )
    print(
        f"S-013 RED  : `.get('success') is not False` calls all {len(green)} targets green, "
        "//:boom included — proto3 omits a false boolean"
    )
    checks += 1

    spans = spans_for(invocation)
    summary = spans[0]
    expect(summary["status"]["code"] == STATUS_ERROR, "the summary span must carry ERROR")
    boom = [span for span in spans if span["name"] == "//:boom"]
    expect(len(boom) == 1 and boom[0]["status"]["code"] == STATUS_ERROR, "//:boom span must be ERROR")
    print("S-013 GREEN: the failure survives into the spans — summary and //:boom both ERROR")
    checks += 1
    return checks


def prove_cache_state() -> int:
    """The four measured cache states, and the control that confuses two of them.

    `cache_states.json` is four real bazel 9.2.0 invocations of one workspace,
    appended into one `--build_event_json_file`-shaped stream. Nothing in it was
    written by hand: the `started`, `targetCompleted`, `testResult` and
    `buildFinished` lines are verbatim, which is why each invocation's own
    `optionsDescription` is still in it and still readable. That matters here
    more than anywhere else in this file, because the surprising row of the
    table — a `cachedRemotely: true` with no remote cache — would otherwise be
    a claim about Bazel that a hand-written fixture simply asserts.

    The four runs, in the order the fixture appends them:

    1. `--remote_cache= --disk_cache=<empty dir>`, fresh server
    2. the same command again against the same live server
    3. `bazel clean && bazel shutdown`, then the same command — now only the
       disk cache can serve
    4. `--remote_cache=http://127.0.0.1:<port> --disk_cache=`, fresh server,
       warmed by a preceding run against a loopback HTTP cache

    Run 4's negative control was run and is *not* in the fixture: with the
    loopback cache stopped, that identical invocation re-executed 6 of 6 with
    `strategy: linux-sandbox`, which is what establishes that run 4's hits came
    from the remote cache rather than from anywhere else.
    """
    checks = 0
    path = FIXTURES / "cache_states.json"
    invocations, findings = read_bep(path)
    expect(findings == [], f"the cache-state fixture must read clean, got {findings}")
    expect(len(invocations) == 4, f"expected 4 invocations, got {len(invocations)}")

    tiers = [{target.cache for target in invocation.targets.values()} for invocation in invocations]
    expect(
        all(len(tier) == 1 for tier in tiers),
        f"each run is one cache state throughout; got {tiers}",
    )
    observed = [tier.pop() for tier in tiers]
    expect(
        sorted(observed) == sorted([CACHE_EXECUTED, CACHE_ACTION, CACHE_DISK, CACHE_REMOTE]),
        f"the four runs must read as four distinct tiers, got {observed}",
    )
    print(
        f"S-016 GREEN: executionInfo.strategy separates all four states — {observed}, "
        "3 targets each, and no two runs share a tier"
    )
    checks += 1

    # The control. Same bytes, the two fields whose names answer the question.
    control = _cached_flag_reader(path)
    expect(len(control) == len(invocations), f"the control must read 4 runs, got {len(control)}")
    disk = observed.index(CACHE_DISK)
    remote = observed.index(CACHE_REMOTE)
    expect(
        set(control[disk].values()) == {CACHE_REMOTE},
        f"the control must call the disk run remote, got {set(control[disk].values())}",
    )
    expect(
        control[disk] == control[remote],
        "the control's whole point is that it cannot tell these two runs apart",
    )
    print(
        f"S-016 RED  : `cachedRemotely` calls run {disk + 1} (`--remote_cache=` empty, "
        f"`--disk_cache` serving) {CACHE_REMOTE!r} — verdict-for-verdict identical to run "
        f"{remote + 1}, which the remote cache really did serve. A lane asserting "
        "'nonzero remote cache hits' through that field passes with the remote cache off"
    )
    checks += 1

    locally = sum(1 for verdict in control[disk].values() if verdict == CACHE_ACTION)
    expect(
        locally == 0 and len(control[disk]) == 3,
        f"the cachedLocally half must call 0 of 3 cached on the disk run, got {locally}",
    )
    print(
        f"S-016 RED  : the `cachedLocally` half of the same control calls {locally} of "
        f"{len(control[disk])} cached on run {disk + 1}, where 3 of 3 were cache hits — "
        "the field is true only for the in-memory action cache, which CI never has"
    )
    checks += 1

    spans = spans_for(invocations[remote])
    attributes = [
        value["stringValue"]
        for span in spans[1:]
        for key, value in ((a["key"], a["value"]) for a in span["attributes"])
        if key == "bazel.cache"
    ]
    expect(
        attributes == [CACHE_REMOTE] * 3,
        f"every target span of the remote run must carry bazel.cache=remote, got {attributes}",
    )
    expect(
        all("bazel.cache" not in [a["key"] for a in span["attributes"]] for span in spans[:1]),
        "bazel.cache is a per-target attribute; the summary span must not carry one",
    )
    print("C-020 GREEN: the tier survives into the spans as bazel.cache on 3 of 3 target spans")
    checks += 1
    return checks


def prove_no_secret() -> int:
    """A planted credential must not reach a span, from any committed fixture."""
    checks = 0
    for name in ("crates_build.json", "failed_build.json"):
        path = FIXTURES / name
        raw = path.read_text(encoding="utf-8")
        plants = raw.count(PLANTED_SECRET)
        expect(
            plants >= 4,
            f"{name} must plant the credential in at least four places, found {plants}",
        )
        invocations, findings = read_bep(path)
        expect(findings == [], f"{name} must read clean, got {findings}")
        document = json.dumps(
            otlp_document([s for i in invocations for s in spans_for(i)], {})
        )
        expect(
            PLANTED_SECRET not in document and "Y2k6c3VwZXJzZWNyZXQ=" not in document,
            f"the planted credential reached a span built from {name}",
        )
        print(f"C-019 GREEN: {name} plants the credential {plants}x; no span carries it")
        checks += 1

    leaked = _wide_reader(FIXTURES / "crates_build.json")
    carriers = [a["key"] for a in leaked if PLANTED_SECRET in a["value"]["stringValue"]]
    expect(
        carriers == ["bazel.optionsDescription"],
        f"the event-level control must leak via optionsDescription, got {carriers}",
    )
    print(
        "C-019 RED  : an event-level allowlist over `started` leaks the credential as "
        "bazel.optionsDescription — which is why READ_FIELDS is per field, not per event"
    )
    checks += 1

    expect(
        set(READ_FIELDS) == {"started", "completed", "testResult", "finished"},
        f"READ_FIELDS names the whole read surface, got {sorted(READ_FIELDS)}",
    )
    expect(
        "optionsDescription" not in READ_FIELDS["started"],
        REFUSED_FIELD_MSG.format(name="optionsDescription"),
    )
    expect(
        not {field for fields in READ_FIELDS.values() for field in fields}
        & {"cachedLocally", "cached_locally", "cachedRemotely", "executionInfo"},
        "neither lying cache field, nor a whole-submessage `executionInfo`, may be allowlisted",
    )
    print(
        "C-019 GREEN: READ_FIELDS admits 4 payloads and 9 fields; optionsDescription, "
        "cachedLocally and cachedRemotely are none of them"
    )
    checks += 1
    return checks


def prove_dropped_span(scratch: Path) -> int:
    """The drop class, both halves, over the transport that actually ships."""
    checks = 0
    invocations, findings = read_bep(FIXTURES / "crates_build.json")
    expect(findings == [], f"the green fixture must read clean, got {findings}")
    spans = [span for invocation in invocations for span in spans_for(invocation)]
    environ = {"OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.invalid:443"}
    url = traces_url(environ) or ""

    accepted, findings = export(spans, url, otlp_headers(environ), 1024, recording_poster())
    expect(accepted == len(spans) and findings == [], f"a clean export must accept all, {findings}")
    print(f"S-013 GREEN: {accepted} of {len(spans)} spans accepted, no findings")
    checks += 1

    accepted, findings = export(spans, url, otlp_headers(environ), 1024, recording_poster(reject=2))
    expect(accepted == len(spans) - 2, f"two rejected spans must be subtracted, got {accepted}")
    code = run_with(scratch, "crates_build.json", recording_poster(reject=2))
    expect(code == 1, f"a partial success must exit 1, got {code}")
    print(
        "S-013 RED  : partialSuccess.rejectedSpans=2 -> exit 1 with "
        f"'{DROP_MSG.format(exported=len(spans) - 2, targets=2)}'"
    )
    checks += 1

    code = run_with(scratch, "crates_build.json", recording_poster(status=503))
    expect(code == 1, f"a 503 must exit 1, got {code}")
    print("S-013 RED  : HTTP 503 -> exit 1 on the transport finding, not a silent zero")
    checks += 1
    return checks


def prove_reader_floor(scratch: Path) -> int:
    """A run that parsed nothing must red, distinguishably from a clean build."""
    checks = 0
    empty = scratch / "empty.json"
    empty.write_text("", encoding="utf-8")
    expect(empty.read_text(encoding="utf-8") == "", "the empty-BEP mutation did not land")
    _, findings = read_bep(empty)
    expect(codes(findings) == ["bep-floor-empty"], f"expected the empty floor, got {codes(findings)}")
    print(f"C-019 RED  : {findings[0].message}")
    checks += 1

    absent = scratch / "does_not_exist.json"
    expect(not absent.exists(), "the absent-BEP precondition does not hold")
    _, findings = read_bep(absent)
    expect(codes(findings) == ["bep-floor-empty"], f"expected the empty floor, got {codes(findings)}")
    checks += 1

    # Events, but no `started` — the "read something, but not a BEP" case, which
    # must not share a message with "read nothing".
    noise = scratch / "noise.json"
    noise.write_text('{"id":{"progress":{}},"progress":{}}\n', encoding="utf-8")
    expect("progress" in noise.read_text(encoding="utf-8"), "the noise mutation did not land")
    _, findings = read_bep(noise)
    expect(
        codes(findings) == ["bep-floor-no-invocation"],
        f"expected the no-invocation floor, got {codes(findings)}",
    )
    print(f"C-019 RED  : {findings[0].message}")
    checks += 1

    invocations, findings = read_bep(FIXTURES / "crates_build.json")
    expect(findings == [], f"the green fixture must read clean, got {findings}")
    short = floor_findings(invocations, MIN_TARGETS)
    expect(
        codes(short) == ["bep-floor-min-targets"],
        f"2 targets under a floor of {MIN_TARGETS} must red, got {codes(short)}",
    )
    print(f"C-019 RED  : {short[0].message}")
    checks += 1

    expect(floor_findings(invocations, 2) == [], "2 targets must clear a floor of 2")
    print(f"C-019 GREEN: 2 targets clear a floor of 2; {MIN_TARGETS} is the mirror's default floor")
    checks += 1
    return checks


def prove_two_invocations(scratch: Path) -> int:
    """One file, two streams — the measured Bazel-retry shape."""
    checks = 0
    first = (FIXTURES / "crates_build.json").read_text(encoding="utf-8")
    second = (FIXTURES / "failed_build.json").read_text(encoding="utf-8")
    both = scratch / "two_invocations.json"
    both.write_text(first + second, encoding="utf-8")
    landed = both.read_text(encoding="utf-8")
    expect(landed.count('"started"') >= 2, "the concatenation did not land: fewer than two starts")

    invocations, findings = read_bep(both)
    expect(findings == [], f"a two-stream file must read clean, got {findings}")
    expect(len(invocations) == 2, f"expected 2 invocations, got {len(invocations)}")
    expect(
        [i.target_count for i in invocations] == [2, 4],
        f"expected 2 then 4 targets, got {[i.target_count for i in invocations]}",
    )
    ids = {invocation.build_id for invocation in invocations}
    expect(len(ids) == 2, "the two invocations must carry distinct build ids")
    traces = {_trace_id(invocation.build_id) for invocation in invocations}
    expect(len(traces) == 2, "two invocations must be two traces, never one")
    print("S-013 GREEN: two appended streams read as 2 invocations / 2 traces / 6 targets")
    checks += 1

    # The red half is the reader that keys on "the" invocation and overwrites.
    last_only = invocations[-1]
    expect(
        last_only.target_count == 4,
        "the control needs the second stream to be the shorter-lived one",
    )
    print(
        "S-013 RED  : a reader keeping only the last `started` reports 4 targets for a "
        "file that holds 6 — half the build, silently"
    )
    checks += 1
    return checks


def prove_schema() -> int:
    """Field by field against the panel queries WP-27 already committed."""
    checks = 0
    invocations, _ = read_bep(FIXTURES / "crates_build.json")
    spans = spans_for(invocations[0])
    document = otlp_document(spans, {})

    resource = document["resourceSpans"][0]["resource"]["attributes"]
    # Literals, not `SERVICE_NAME` / `REPOSITORY_NAME`: a proof that reads the
    # constant it checks greens on any value the constant is changed to.
    expect(
        {"key": "service.name", "value": {"stringValue": "ocx-mirror-bazel-build"}} in resource,
        f"the dashboards' repo=ocx-mirror value is service.name=ocx-mirror-bazel-build, "
        f"got {resource}",
    )
    expect(
        {"key": "vcs.repository.name", "value": {"stringValue": "ocx-mirror"}} in resource,
        f"every mirror BEP span must carry resource vcs.repository.name=ocx-mirror, got {resource}",
    )

    def attrs(span: dict[str, Any]) -> dict[str, Any]:
        return {a["key"]: a["value"] for a in span["attributes"]}

    summary = attrs(spans[0])
    expect(summary["bazel.kind"] == {"stringValue": "summary"}, "panel 3 selects bazel.kind=summary")
    expect("bazel.build_id" in summary, "panel 4 groups by span.bazel.build_id")
    expect(
        summary["bazel.target_count"] == {"intValue": "2"},
        f"panel 4 sums span.bazel.target_count as a number, got {summary['bazel.target_count']}",
    )
    targets = [attrs(span) for span in spans[1:]]
    expect(
        all(a["bazel.kind"] == {"stringValue": "target"} for a in targets),
        "panel 4 counts bazel.kind=target",
    )
    expect(
        {a["bazel.build_id"]["stringValue"] for a in targets}
        == {summary["bazel.build_id"]["stringValue"]},
        "every target span must carry the summary's build_id, or panel 4 groups them apart",
    )
    expect(
        int(summary["bazel.target_count"]["intValue"]) == len(targets),
        "declared must equal landed on a clean export, or S-013 reds on a correct build",
    )
    print(
        "C-020 GREEN: service.name=ocx-mirror-bazel-build + vcs.repository.name=ocx-mirror; "
        "1 summary span with bazel.build_id + "
        "bazel.target_count=2 (intValue); 2 target spans sharing that build_id"
    )
    checks += 1

    hexadecimal = "0123456789abcdef"
    expect(len(spans[0]["traceId"]) == 32, "traceId must be 32 hex, not base64")
    expect(len(spans[1]["spanId"]) == 16, "spanId must be 16 hex, not base64")
    expect(spans[1]["parentSpanId"] == spans[0]["spanId"], "target spans hang off the summary")
    expect("parentSpanId" not in spans[0], "the summary span is the trace root")
    expect(
        all(character in hexadecimal for character in spans[0]["traceId"]),
        "OTLP/JSON departs from the protobuf mapping: ids are hex, never base64",
    )
    print("C-020 GREEN: ids are hex (32/16), summary is the root, targets are its children")
    checks += 1

    # The other branch of `_trace_id`. BEP publishes no stability guarantee, so
    # the day an invocation id stops being a UUID the *fallback* is what Tempo
    # receives — and an unguarded fallback is a branch whose first run is in
    # production. Both halves asserted on inputs this line controls.
    for odd in ("not-a-uuid", "", "75ABC818AA1143C7AE2CDF8B1C85A76D", "x" * 32):
        derived = _trace_id(odd)
        expect(
            len(derived) == 32 and all(character in hexadecimal for character in derived),
            f"_trace_id({odd!r}) produced {derived!r}, which is not 32 lowercase hex",
        )
    expect(
        _trace_id("75ABC818-AA11-43C7-AE2C-DF8B1C85A76D") == _trace_id(
            "75abc818-aa11-43c7-ae2c-df8b1c85a76d"
        ),
        "a upper-case invocation id must land in the same trace as its lower-case spelling",
    )
    print("C-020 GREEN: _trace_id yields 32 lowercase hex for a UUID and for four non-UUIDs")
    checks += 1
    return checks


def prove_provenance() -> int:
    """A CI build and a workstation build must be separable on the dashboard.

    The red half is the resource this file shipped until now: one attribute,
    identical on every host, so the panels showed a runner's regression and
    somebody's local experiment as one series with no filter that splits them.
    """
    checks = 0
    invocations, _ = read_bep(FIXTURES / "crates_build.json")
    spans = spans_for(invocations[0])

    ci = {
        "GITHUB_RUN_ID": "35679328733",
        "GITHUB_REPOSITORY": "ocx-sh/ocx-mirror",
        "GITHUB_SERVER_URL": "https://github.com/",
    }
    attributes = {
        a["key"]: a["value"]["stringValue"]
        for a in otlp_document(spans, ci)["resourceSpans"][0]["resource"]["attributes"]
    }
    expect(
        attributes.get("ocx.source") == "ci",
        f"a run under GitHub Actions must carry ocx.source=ci, got {attributes}",
    )
    expect(
        attributes.get("ci.run_url")
        == "https://github.com/ocx-sh/ocx-mirror/actions/runs/35679328733",
        f"the run URL must be clickable from the panel, got {attributes.get('ci.run_url')}",
    )
    print(
        "C-019 GREEN: a build under GitHub Actions carries ocx.source=ci and a ci.run_url "
        "resolving to its run"
    )
    checks += 1

    local = {a["key"]: a["value"]["stringValue"] for a in resource_attributes({})}
    expect(
        local.get("ocx.source") == "local" and "ci.run_url" not in local,
        f"a workstation build must be ocx.source=local with no run URL, got {local}",
    )
    expect(
        local.get("service.name") == "ocx-mirror-bazel-build"
        and local.get("vcs.repository.name") == "ocx-mirror",
        f"a workstation build must still carry the mirror's service.name and "
        f"vcs.repository.name, got {local}",
    )
    print(
        "C-019 RED  : with GITHUB_RUN_ID and GITHUB_REPOSITORY absent the same code path "
        "reports ocx.source=local and no ci.run_url, so the two series never merge"
    )
    checks += 1

    half = resource_attributes({"GITHUB_RUN_ID": "1", "GITHUB_SERVER_URL": "https://x"})
    expect(
        {a["key"] for a in half} == {"service.name", "vcs.repository.name", "ocx.source"}
        and {a["key"]: a["value"]["stringValue"] for a in half}["ocx.source"] == "local",
        f"a run id without a repository cannot name a URL and must stay local, got {half}",
    )
    print(
        "C-019 RED  : a run id with no GITHUB_REPOSITORY yields no half-built ci.run_url"
    )
    checks += 1
    return checks


def prove_silent_exit(scratch: Path) -> int:
    """Unset endpoint is the only silent exit, and it must read nothing at all."""
    checks = 0
    sink: list[bytes] = []
    code = run(
        FIXTURES / "crates_build.json",
        MIN_TARGETS,
        1024,
        {},
        recording_poster(sink=sink),
    )
    expect(code == 0, f"an unset endpoint must exit 0, got {code}")
    expect(sink == [], "an unset endpoint must post nothing")
    print("C-019 GREEN: OTEL_EXPORTER_OTLP_ENDPOINT unset -> exit 0, silent, zero spans posted")
    checks += 1

    # And the floor it skipped must still be live when the endpoint is set —
    # otherwise the silent exit would be a way to never run the check at all.
    code = run_with(scratch, "crates_build.json", recording_poster(), minimum=MIN_TARGETS)
    expect(code == 1, f"with an endpoint set, 2 targets under a floor of {MIN_TARGETS} must exit 1, got {code}")
    print("C-019 RED  : with the endpoint set, the same input exits 1 on the floor")
    checks += 1

    expect(
        traces_url({"OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.ocx.sh:443"})
        == "https://otel.ocx.sh:443/v1/traces",
        "the endpoint must resolve to nginx's `location /v1/`",
    )
    expect(
        otlp_headers({"OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Basic YWJjOmRlZg=="})[
            "Authorization"
        ]
        == "Basic YWJjOmRlZg==",
        "base64 padding must survive the header split",
    )
    print("C-019 GREEN: endpoint -> /v1/traces; `k=v` split on the first `=` keeps b64 padding")
    checks += 1
    return checks


def run_with(scratch: Path, fixture: str, post: Poster, minimum: int = 1) -> int:
    """`run` against a committed fixture with a stub transport, output muted."""
    del scratch
    environ = {
        "OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.invalid:443",
        "OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Basic YWJjOmRlZg==",
    }
    return run(FIXTURES / fixture, minimum, 1024, environ, post)


def prove_live_transport(scratch: Path) -> int:
    """The real `post_otlp`, against a loopback server this test owns.

    Every other proof here stubs the transport, so every other green is a green
    about the mapping and not about the wire. This is the one that runs the
    shipped `urllib` path end to end — on 127.0.0.1, never the realm C-029
    keeps the proof corpus out of.
    """
    del scratch
    import http.server
    import threading

    received: list[tuple[str, bytes]] = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            length = int(self.headers.get("Content-Length", "0"))
            received.append((self.path, self.rfile.read(length)))
            body = json.dumps({"partialSuccess": {}}).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args: object) -> None:
            return

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = server.server_address[1]
        environ = {
            "OTEL_EXPORTER_OTLP_ENDPOINT": f"http://127.0.0.1:{port}",
            "OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Basic YWJjOmRlZg==",
        }
        code = run(FIXTURES / "crates_build.json", 2, 1024, environ, post_otlp)
    finally:
        server.shutdown()
        server.server_close()

    expect(code == 0, f"the loopback export must exit 0, got {code}")
    expect(len(received) == 1, f"expected one POST, got {len(received)}")
    path, body = received[0]
    expect(path == "/v1/traces", f"the collector path must be /v1/traces, got {path}")
    document = json.loads(body)
    spans = document["resourceSpans"][0]["scopeSpans"][0]["spans"]
    expect(len(spans) == 3, f"1 summary + 2 targets must reach the wire, got {len(spans)}")
    expect(PLANTED_SECRET not in body.decode("utf-8"), "the credential reached the wire")
    print(
        "C-019 GREEN: the shipped post_otlp path POSTs 3 spans as application/json to "
        "/v1/traces on loopback, credential-free, and reads partialSuccess back"
    )
    return 1 + prove_no_redirect()


def prove_no_redirect() -> int:
    """`Authorization` must not be replayed at a redirect target (CWE-200).

    Two loopback servers: the first answers `302` pointing at the second, the
    second records whatever headers reach it. The control is the *default*
    opener, which forwards the credential — without it this proof could not tell
    a refused redirect from a urllib that never forwarded one, and the fix would
    be a decoration.
    """
    import http.server
    import threading

    seen: list[str | None] = []

    class Sink(http.server.BaseHTTPRequestHandler):
        def _record(self) -> None:
            seen.append(self.headers.get("Authorization"))
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.end_headers()

        # Both verbs: urllib rewrites a 302'd POST into a GET (RFC 7231 §6.4.3,
        # "for historical reasons"), so recording only `do_POST` would report an
        # untouched sink for a request that did arrive.
        do_GET = _record
        do_POST = _record

        def log_message(self, *args: object) -> None:
            return

    sink = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Sink)
    threading.Thread(target=sink.serve_forever, daemon=True).start()
    sink_port = sink.server_address[1]

    class Redirector(http.server.BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            # 302 and not 307: urllib refuses to auto-follow a 307 POST at all,
            # so a 307 here would green through the *default* opener too and the
            # control below would prove the opposite of what it exists to prove.
            self.send_response(302)
            self.send_header("Location", f"http://127.0.0.1:{sink_port}/v1/traces")
            self.send_header("Content-Length", "0")
            self.end_headers()

        def log_message(self, *args: object) -> None:
            return

    redirector = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Redirector)
    threading.Thread(target=redirector.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{redirector.server_address[1]}/v1/traces"
    headers = {"Content-Type": "application/json", "Authorization": "Basic YWJjOmRlZg=="}

    try:
        # --- the control: the default opener, which is what `urlopen` installs.
        request = urllib.request.Request(url, data=b"{}", headers=headers, method="POST")
        try:
            with urllib.request.urlopen(request, timeout=10):
                pass
        except urllib.error.URLError as error:  # pragma: no cover - loopback only
            raise SystemExit(f"bep_to_otlp self-test: the control request failed: {error}") from error
        expect(
            seen == ["Basic YWJjOmRlZg=="],
            f"the default opener did NOT forward the credential across the redirect ({seen}) — "
            "this urllib does not have the behaviour the fix guards against, so the green "
            "below would prove nothing and this proof must be re-read, not deleted",
        )
        print(f"C-019 CONTROL: urllib's default opener replayed {seen[0]!r} at the redirect target")

        # --- the shipped path: the 3xx becomes a transport error, and the sink
        #     never sees a second request at all.
        seen.clear()
        raised = ""
        try:
            post_otlp(url, headers, b"{}")
        except urllib.error.HTTPError as error:
            raised = f"HTTP {error.code}"
        expect(raised.startswith("HTTP 3"), f"post_otlp must surface the 3xx, got {raised!r}")
        expect(seen == [], f"the credential reached the redirect target anyway: {seen}")
    finally:
        for server in (redirector, sink):
            server.shutdown()
            server.server_close()

    print(
        f"C-019 GREEN: post_otlp refuses the same redirect ({raised}) and the target sees no "
        "request — OTLP/HTTP defines no redirect, so nothing is lost by refusing one"
    )
    return 2


#: One BEP line in the shape Bazel's proto3-JSON printer emits it: `=` escaped,
#: which is the whole point of the scanner leg below.
_PLANTED_ENV_VALUE = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
_ESCAPED_BEP_LINE = (
    '{"id":{"structuredCommandLine":{"commandLineLabel":"canonical"}},'
    '"structuredCommandLine":{"sections":[{"chunkList":{"chunk":['
    '"--client_env\\u003dAWS_SECRET_ACCESS_KEY\\u003d' + _PLANTED_ENV_VALUE + '",'
    '"--remote_header\\u003dauthorization\\u003dBasic Y2k6c3VwZXJzZWNyZXQ\\u003d"]}}]}}'
)


def prove_bep_not_persisted() -> int:
    """R-2: the stream never survives the task, and the obvious scanner lies.

    Two legs. The first is the *scanner*: over bytes shaped the way Bazel
    writes them, a grep for `--client_env=` reports clean while the secret is
    right there. The second is the *recipe*: the live taskfile and workflow
    corpus, then the same bytes with the path moved back under `target/` and
    with the `defer:` removed.
    """
    checks = 0

    naive = _ESCAPED_BEP_LINE.count("--client_env=")
    escaped = _ESCAPED_BEP_LINE.count("--client_env\\u003d")
    expect(naive == 0, f"the planted line must defeat the naive spelling, it matched {naive}x")
    expect(escaped >= 1, "the planted line must carry the escaped spelling")
    expect(
        _PLANTED_ENV_VALUE in _ESCAPED_BEP_LINE,
        "the planted line must carry the credential value the scanner is meant to find",
    )
    decoded = json.dumps(json.loads(_ESCAPED_BEP_LINE))
    expect(
        f"AWS_SECRET_ACCESS_KEY={_PLANTED_ENV_VALUE}" in json.loads(decoded)["structuredCommandLine"]
        ["sections"][0]["chunkList"]["chunk"][0],
        "the decoded chunk must hold KEY=VALUE — the escaping is on the wire, not in the data",
    )
    print(
        f"R-2 RED   : a scanner keyed on `--client_env=` matches {naive}x over a line carrying "
        f"the credential; the escaped spelling it would have to use matches {escaped}x"
    )
    checks += 1

    # The mirror's corpus: its one BEP producer (`bazel:build:nobuild`, plus
    # `bazel:test:unit`'s floor stream) and the workflow that calls it.
    taskfile = REPO_ROOT / "taskfiles" / "bazel.taskfile.yml"
    workflow = REPO_ROOT / ".github" / "workflows" / "verify.yml"
    live = {path.name: path.read_text(encoding="utf-8") for path in (taskfile, workflow)}
    recipe = live[taskfile.name]
    expect(
        re.search(r"^  build:nobuild:", recipe, re.MULTILINE),
        f"{taskfile.relative_to(REPO_ROOT)} has no `build:nobuild:` task — the BEP producer "
        "this proof reads was renamed or dropped",
    )
    expect(
        any(BEP_ARG.search(text) for text in live.values()),
        "`build:nobuild` exists but no source in the live corpus names a build event "
        "stream — the producer dropped `--build_event_json_file`, or the reader drifted",
    )
    findings = bep_persistence_findings(live)
    expect(findings == [], f"the live corpus must be silent, got {[f.message for f in findings]}")
    print(
        f"R-2 GREEN : {len(live)} live source(s) — every `--build_event_json_file=` is rooted "
        f"at a `mktemp -d` outside the checkout and a `defer:` removes it"
    )
    checks += 1

    in_tree = recipe.replace("{{.BEP_DIR}}/bep.json", "{{.ROOT_DIR}}/target/bep.json")
    expect(
        in_tree != recipe,
        "the in-workspace mutation did not land — the producer no longer spells its "
        "stream `{{.BEP_DIR}}/bep.json`, or dropped it",
    )
    red = bep_persistence_findings({taskfile.name: in_tree})
    expect(codes(red) == ["bep-in-workspace"], f"got {codes(red)}")
    print(f"R-2 RED   : {red[0].message[:150]}")
    checks += 1

    undeleted = "\n".join(
        line for line in recipe.splitlines() if "defer: rm -rf" not in line
    )
    expect(
        SCRATCH_DEFER.search(recipe) and not SCRATCH_DEFER.search(undeleted),
        "the cleanup mutation did not land",
    )
    red = bep_persistence_findings({taskfile.name: undeleted})
    expect(codes(red) == ["bep-not-deleted"], f"got {codes(red)}")
    print(f"R-2 RED   : {red[0].message[:150]}")
    checks += 1

    revived = bep_persistence_findings(
        {"verify.yml": "      run: python3 scripts/bep_to_otlp.py --bep target/bep.json\n"}
    )
    expect(
        codes(revived) == ["bep-in-workspace", "bep-not-deleted"],
        f"a CI step reading the old path must red on both counts, got {codes(revived)}",
    )
    print(f"R-2 RED   : {revived[0].message[:150]}")
    checks += 1
    return checks


def self_test() -> int:
    """Every pair, on fixtures this repository owns."""
    scratch = REPO_ROOT / ".tmp"
    scratch.mkdir(mode=0o700, exist_ok=True)
    checks = 0
    import tempfile

    with tempfile.TemporaryDirectory(dir=scratch) as directory:
        work = Path(directory)
        checks += prove_failed_build()
        checks += prove_cache_state()
        checks += prove_no_secret()
        checks += prove_dropped_span(work)
        checks += prove_reader_floor(work)
        checks += prove_two_invocations(work)
        checks += prove_schema()
        checks += prove_provenance()
        checks += prove_silent_exit(work)
        checks += prove_live_transport(work)
        checks += prove_bep_not_persisted()
    print(
        f"bep_to_otlp self-test: {checks} checks passed — a failing build parsed, a dropped "
        "span caught, no span carrying the planted credential, the stream itself held outside "
        "the checkout and deleted from a `defer:`, the reader floor red on an "
        "empty and on a target-poor BEP, two appended streams read whole, WP-27's panel "
        "schema and the mirror's service.name + vcs.repository.name asserted field by "
        "field, a CI build separable from a workstation one by "
        "ocx.source and ci.run_url, and four real cache states separated by "
        "executionInfo.strategy where cachedRemotely cannot tell two of them apart"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true", help="prove every pair red and green")
    mode.add_argument("--bep", type=Path, help="the --build_event_json_file to read")
    parser.add_argument(
        "--min-targets",
        type=int,
        default=MIN_TARGETS,
        help=f"reader floor on TargetComplete events (default {MIN_TARGETS}, "
        "7 crates/* libraries + the root library)",
    )
    parser.add_argument(
        "--batch",
        type=int,
        default=1024,
        help="spans per POST (default 1024)",
    )
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    return run(args.bep, args.min_targets, args.batch, dict(os.environ), post_otlp)


if __name__ == "__main__":
    sys.exit(main())
