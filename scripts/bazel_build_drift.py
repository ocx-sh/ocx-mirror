#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`task bazel:build:drift` — every Cargo dep edge has a BUILD twin and back.

    scripts/bazel_build_drift.py              # gate the live tree
    scripts/bazel_build_drift.py --self-test  # red/green proofs, no subprocess

Port of ocx's `scripts/bazel_build_drift.py` (adr_bazel_crate_split.md § C7).
Two readings, compared per workspace package:

* Bazel: `bazel query 'kind("rust_", //...)' --output=build` — the rules as
  loaded, after `all_crate_deps()` expanded. The BUILD text holds no
  third-party list at all, so a text parse would agree with itself forever.
* Cargo: `cargo metadata --locked` resolve graph — feature- and
  rename-resolved, the same lock crate_universe renders `@crates//` from.

Differences from ocx, both deliberate:

* No committed label map. `@crates//<name>-<version>:…` resolves against the
  metadata's `<name>-<version>` stems at run time (never a `rsplit("-")`:
  versions may contain `-`). A label matching no stem is its own finding.
* Edges compare per kind. Library/binary rules vs Cargo normal edges,
  exactly. `rust_test` rules (a package that has one) vs dev edges: every
  Cargo dev edge has a test twin, and every test edge exists in Cargo as
  normal or dev. A package without a `rust_test` has no dev comparison.
  A dev edge that is also a normal edge (a dev row adding a feature, e.g.
  tokio `test-util`) is twinned by the library's edge: a `crate =` test
  inherits the library's deps, and `all_crate_deps(normal_dev = True)` omits
  a crate that is already normal.
  Build-kind edges are excluded: no `cargo_build_script` exists here.

Also: every `rust_library`/`rust_binary` carries `version` == the workspace
version, and a reader floor — every Cargo member must own >= 1 rust rule, and
at least MIN_MEMBERS members must exist — so an empty read is never green.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
QUERY = 'kind("rust_", //...)'
#: Root package + seven `crates/ocx_mirror_*` (adr_bazel_crate_split.md § C1).
MIN_MEMBERS = 8

BUILD_HEADER = re.compile(r"^# (?P<dir>/.*?)/?BUILD\.bazel:\d+:\d+$")
RULE_OPEN = re.compile(r"^(?P<kind>[a-z_][a-z0-9_]*)\($")
RULE_CLOSE = re.compile(r"^\)$")
DEP_ATTR = re.compile(r"^  (?:deps|proc_macro_deps) = (?P<value>.*)$")
VERSION_ATTR = re.compile(r'^  version = "(?P<value>[^"]*)",$')
NAME_ATTR = re.compile(r'^  name = "(?P<value>[^"]*)",$')
STRING_LITERAL = re.compile(r'"([^"]*)"')
DEP_LABEL_PREFIX = ("@crates//", "//")
VERSIONED_KINDS = ("rust_library", "rust_binary", "rust_proc_macro")


@dataclasses.dataclass(frozen=True)
class Finding:
    code: str
    message: str


@dataclasses.dataclass
class BuildRead:
    normal: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    test: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    versions: list[tuple[str, str, str | None]] = dataclasses.field(default_factory=list)
    targets: int = 0
    orphan_rules: int = 0


@dataclasses.dataclass
class CargoRead:
    normal: dict[str, set[str]]
    dev: dict[str, set[str]]
    own_name: dict[str, str]
    stems: dict[str, str]
    version: str


def pkg_label(package: str) -> str:
    return f"//{package}"


def read_build_output(text: str, root: str) -> BuildRead:
    """`--output=build` text -> per-package edge sets, split library/test.

    Each rule is preceded by `# <abs dir>/BUILD.bazel:<line>:<col>`; the
    package is that directory relative to `root` ("" for the root package).
    The package resets at each `)`, so a rule with no header of its own is an
    orphan rather than silently filed under the previous rule's package.
    """
    read = BuildRead()
    root = root.rstrip("/")
    package: str | None = None
    kind = name = version = None
    for line in text.splitlines():
        header = BUILD_HEADER.match(line)
        if header is not None:
            directory = header.group("dir").rstrip("/")
            if directory == root:
                package = ""
            elif directory.startswith(root + "/"):
                package = directory[len(root) + 1 :]
            else:
                package = None
            continue
        opener = RULE_OPEN.match(line)
        if opener is not None:
            read.targets += 1
            kind, name, version = opener.group("kind"), None, None
            if package is None:
                read.orphan_rules += 1
            else:
                read.normal.setdefault(package, set())
            continue
        if RULE_CLOSE.match(line):
            if package is not None and kind in VERSIONED_KINDS:
                read.versions.append((package, name or "?", version))
            package = kind = None
            continue
        if package is None:
            continue
        if match := NAME_ATTR.match(line):
            name = match.group("value")
        elif match := VERSION_ATTR.match(line):
            version = match.group("value")
        elif match := DEP_ATTR.match(line):
            bucket = read.test if kind == "rust_test" else read.normal
            labels = bucket.setdefault(package, set())
            for literal in STRING_LITERAL.findall(match.group("value")):
                if literal.startswith(DEP_LABEL_PREFIX):
                    labels.add(literal)
    return read


def read_cargo_metadata(document: dict) -> CargoRead:
    by_id = {package["id"]: package for package in document["packages"]}
    nodes = {node["id"]: node for node in document["resolve"]["nodes"]}
    root = document["workspace_root"].rstrip("/")
    normal: dict[str, set[str]] = {}
    dev: dict[str, set[str]] = {}
    own_name: dict[str, str] = {}
    version = ""
    for member in document["workspace_members"]:
        package = by_id[member]
        directory = str(Path(package["manifest_path"]).parent)
        key = "" if directory == root else directory[len(root) + 1 :]
        own_name[key] = package["name"]
        if key == "":
            version = package["version"]
        normal[key], dev[key] = set(), set()
        for dependency in nodes[member]["deps"]:
            dep_name = by_id[dependency["pkg"]]["name"]
            kinds = {entry["kind"] for entry in dependency["dep_kinds"]}
            if None in kinds:
                normal[key].add(dep_name)
            if "dev" in kinds:
                dev[key].add(dep_name)
    stems = {f"{p['name']}-{p['version']}": p["name"] for p in document["packages"]}
    return CargoRead(normal=normal, dev=dev, own_name=own_name, stems=stems, version=version)


def resolve(label: str, cargo: CargoRead) -> str | None:
    """Bazel label -> Cargo package name; None when nothing matches."""
    if label.startswith("@crates//"):
        return cargo.stems.get(label[len("@crates//") :].split(":", 1)[0])
    target = label.split(":", 1)[1] if ":" in label else label.rsplit("/", 1)[-1]
    return target if target in cargo.own_name.values() else None


def check_drift(*, build_text: str, metadata: dict, root: str) -> list[Finding]:
    read = read_build_output(build_text, root)
    cargo = read_cargo_metadata(metadata)
    findings: list[Finding] = []

    if read.orphan_rules:
        findings.append(
            Finding("drift-orphan-rule", f"BUILD drift: {read.orphan_rules} rule(s) carry no BUILD.bazel header under {root} — their edges were read by nobody")
        )
    missing_pkgs = sorted(set(cargo.own_name) - set(read.normal))
    if len(cargo.own_name) < MIN_MEMBERS or missing_pkgs:
        findings.append(
            Finding(
                "drift-reader-floor",
                f"BUILD drift: read {len(read.normal)} Bazel packages / {read.targets} rust rules against "
                f"{len(cargo.own_name)} Cargo members (floor {MIN_MEMBERS}); members with no rust rule: "
                f"{[pkg_label(p) for p in missing_pkgs]}",
            )
        )

    def mapped(package: str, labels: set[str]) -> set[str]:
        self_edge = f"//{package}:{cargo.own_name.get(package)}"
        names: set[str] = set()
        for label in sorted(labels - {self_edge}):
            name = resolve(label, cargo)
            if name is None:
                findings.append(
                    Finding("drift-unmapped", f"BUILD drift: {label} matches no Cargo package (`<name>-<version>` stem or workspace member) — a stale pin or a label spelled by hand")
                )
            else:
                names.add(name)
        return names

    for package in sorted(set(cargo.own_name) | set(read.normal)):
        where = pkg_label(package)
        declared = cargo.normal.get(package, set())
        built = mapped(package, read.normal.get(package, set()))
        missing, extra = sorted(declared - built), sorted(built - declared)
        if missing or extra:
            findings.append(
                Finding("drift-set", f"BUILD drift: {where} normal edges — in Cargo not in BUILD: {missing}; in BUILD not in Cargo: {extra}")
            )
        if package in read.test:
            tested = mapped(package, read.test[package])
            dev = cargo.dev.get(package, set())
            missing = sorted(dev - tested - declared)
            extra = sorted(tested - dev - declared)
            if missing or extra:
                findings.append(
                    Finding("drift-dev-set", f"BUILD drift: {where} rust_test edges — dev edge in Cargo not in BUILD: {missing}; in BUILD not in Cargo: {extra}")
                )

    for package, name, version in read.versions:
        if version != cargo.version:
            findings.append(
                Finding("drift-version", f"BUILD drift: {pkg_label(package)}:{name} version = {version!r}, workspace version is {cargo.version!r}")
            )
    return list(dict.fromkeys(findings))


def run(command: list[str]) -> tuple[str, Finding | None]:
    try:
        result = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", check=False, timeout=900, cwd=REPO_ROOT)
    except (OSError, subprocess.TimeoutExpired) as error:
        return "", Finding("drift-reader", f"BUILD drift: `{' '.join(command)}` failed: {error}")
    if result.returncode != 0:
        return "", Finding("drift-reader", f"BUILD drift: `{' '.join(command)}` exited {result.returncode}: {result.stderr[-400:]}")
    return result.stdout, None


def report(findings: list[Finding]) -> int:
    sys.stdout.flush()
    for finding in findings:
        print(finding.message, file=sys.stderr)
    if not findings:
        print("bazel:build:drift: every Cargo edge has a BUILD twin and back; version attrs match")
    return 1 if findings else 0


# ---------------------------------------------------------------------------
# Self-test — synthetic readings in the grammar bazel 9.2.0 prints, no subprocess.
# ---------------------------------------------------------------------------

ROOT = "/abs/ws"
TOKIO = "@crates//tokio-1.52.3:tokio-1.52.3"
SERDE = "@crates//serde-1.0.228:serde-1.0.228"
TOML = "@crates//toml-1.1.4+spec-1.1.0:toml-1.1.4+spec-1.1.0"
OCX_OCI = "@crates//ocx_oci-0.6.2:ocx_oci-0.6.2"
TEMPFILE = "@crates//tempfile-3.27.0:tempfile-3.27.0"
BUILD_SCRIPT_ONLY = ("cc", "1.2.0")
MEMBERS = [
    ("", "ocx_mirror"),
    ("crates/ocx_mirror_http", "ocx_mirror_http"),
    ("crates/ocx_mirror_report", "ocx_mirror_report"),
    ("crates/ocx_mirror_source", "ocx_mirror_source"),
    ("crates/ocx_mirror_error", "ocx_mirror_error"),
    ("crates/ocx_mirror_spec", "ocx_mirror_spec"),
    ("crates/ocx_mirror_pipeline", "ocx_mirror_pipeline"),
    ("crates/ocx_mirror_test_support", "ocx_mirror_test_support"),
]


def expect(condition: bool, problem: str) -> None:
    """A loud exit — a bare `assert` vanishes under `python3 -O`."""
    if not condition:
        raise SystemExit(f"bazel build drift self-test: {problem}")


def _rule(package: str, kind: str, name: str, deps: list[str], version: str | None = "0.6.2") -> str:
    lines = [f"# {ROOT}/{package + '/' if package else ''}BUILD.bazel:9:13", f"{kind}(", f'  name = "{name}",']
    lines.append("  deps = [" + ", ".join(f'"{d}"' for d in deps) + "],")
    if version is not None:
        lines.append(f'  version = "{version}",')
    lines += [")", f"# Rule {name} instantiated at (most recent call last):", f"#   {ROOT}/BUILD.bazel:9:13 in <toplevel>", ""]
    return "\n".join(lines)


def sample_tree() -> tuple[str, dict]:
    third = {TOKIO: ("tokio", "1.52.3"), SERDE: ("serde", "1.0.228"), TOML: ("toml", "1.1.4+spec-1.1.0"),
             OCX_OCI: ("ocx_oci", "0.6.2"), TEMPFILE: ("tempfile", "3.27.0")}
    packages = [{"id": label, "name": n, "version": v, "manifest_path": "/x/Cargo.toml"} for label, (n, v) in third.items()]
    packages.append({"id": "cc", "name": BUILD_SCRIPT_ONLY[0], "version": BUILD_SCRIPT_ONLY[1], "manifest_path": "/x/Cargo.toml"})
    records, nodes, members = [], [], []
    for index, (package, name) in enumerate(MEMBERS):
        normal = [TOKIO, SERDE if index % 2 else TOML]
        first_party = [] if index in (1, 7) else ["ocx_mirror_http"]
        if package == "crates/ocx_mirror_http":
            normal.append(OCX_OCI)
        labels = normal + [f"//crates/{fp}:{fp}" for fp in first_party]
        records.append(_rule(package, "rust_library", name, labels))
        dev = []
        if index != 7:
            dev = [TEMPFILE, "//crates/ocx_mirror_test_support:ocx_mirror_test_support"]
            # A test target also names its own lib (the self-edge) and a normal edge.
            records.append(_rule(package, "rust_test", f"{name}_test", dev + [f"//{package}:{name}", TOKIO], version=None))
        if package == "":
            records.append(_rule("", "rust_binary", "ocx-mirror", normal + ["//:ocx_mirror"]))
        member_id = f"member::{name}"
        members.append(member_id)
        packages.append({"id": member_id, "name": name, "version": "0.6.2",
                         "manifest_path": f"{ROOT}/{package + '/' if package else ''}Cargo.toml"})
        deps = [{"pkg": label, "dep_kinds": [{"kind": None}]} for label in normal]
        deps += [{"pkg": f"member::{fp}", "dep_kinds": [{"kind": None}]} for fp in first_party]
        if dev:
            deps += [{"pkg": TEMPFILE, "dep_kinds": [{"kind": "dev"}]},
                     {"pkg": "member::ocx_mirror_test_support", "dep_kinds": [{"kind": "dev"}]}]
        deps.append({"pkg": "cc", "dep_kinds": [{"kind": "build"}]})
        nodes.append({"id": member_id, "deps": deps})
    metadata = {"packages": packages, "workspace_members": members, "workspace_root": ROOT, "resolve": {"nodes": nodes}}
    return "\n".join(records), metadata


def _in_package(text: str, package: str, old: str, new: str) -> str:
    """Replace `old` inside one package's records only (header-delimited)."""
    header = f"# {ROOT}/{package + '/' if package else ''}BUILD.bazel:"
    blocks = text.split("\n# " + ROOT)
    out = [blocks[0]]
    for block in blocks[1:]:
        whole = "# " + ROOT + block
        out.append(whole.replace(old, new) if whole.startswith(header) else whole)
    return "\n".join(out)


def self_test() -> int:
    text, metadata = sample_tree()
    checks = 0

    def run_case(label: str, build: str, meta: dict, want: list[str], needle: str = "") -> None:
        nonlocal checks
        got = check_drift(build_text=build, metadata=meta, root=ROOT)
        codes = sorted({f.code for f in got})
        expect(codes == want, f"{label}: expected {want}, got {codes}: {[f.message for f in got]}")
        if needle:
            expect(any(needle in f.message for f in got), f"{label}: no finding names {needle!r}: {[f.message for f in got]}")
        print(f"{'GREEN' if not want else 'RED  '}: {label}" + (f" — {got[0].message}" if got else ""))
        checks += 1

    read = read_build_output(text, ROOT)
    expect(set(read.normal) == {p for p, _ in MEMBERS}, f"reader packages {sorted(read.normal)}")
    expect("" in read.normal and "//crates/ocx_mirror_http:ocx_mirror_http" in read.normal[""], "root package not read")
    run_case("agreeing tree (root + 7 crates, bin self-edge, test self-edge, build-kind dep excluded)", text, metadata, [])

    selected = f'# {ROOT}/BUILD.bazel:1:1\nrust_library(\n  deps = [] + select({{"@rules_rust//rust/platform:x86_64-pc-windows-msvc": ["{TOKIO}"], "//conditions:default": []}}),\n)\n'
    picked = {x for x in read_build_output(selected, ROOT).normal[""] if resolve(x, read_cargo_metadata(metadata))}
    expect(picked == {TOKIO}, f"select() branch labels not read cleanly: {picked}")
    print("GREEN: a select() deps line yields its branch labels; its keys resolve to nothing")
    checks += 1

    edge = '"//crates/ocx_mirror_http:ocx_mirror_http"'
    mutated = _in_package(text, "crates/ocx_mirror_spec", edge + "]", "]").replace(", ]", "]")
    expect(mutated != text, "first-party drop did not land")
    run_case("first-party edge dropped from BUILD", mutated, metadata, ["drift-set"], "in Cargo not in BUILD: ['ocx_mirror_http']")

    mutated = _in_package(text, "crates/ocx_mirror_http", f', "{OCX_OCI}"', "")
    expect(mutated != text, "@crates drop did not land")
    run_case("@crates// edge (an ocx crate) dropped from BUILD", mutated, metadata, ["drift-set"], "in Cargo not in BUILD: ['ocx_oci']")

    cut = json.loads(json.dumps(metadata))
    node = next(n for n in cut["resolve"]["nodes"] if n["id"] == "member::ocx_mirror_spec")
    before = len(node["deps"])
    node["deps"] = [d for d in node["deps"] if d["pkg"] != TOKIO]
    expect(len(node["deps"]) == before - 1, "cargo-side removal did not land")
    # The test target names tokio too, so both kinds red — each on its own line.
    run_case("edge in BUILD not in Cargo", text, cut, ["drift-dev-set", "drift-set"], "in BUILD not in Cargo: ['tokio']")

    bumped = text.replace(TOKIO, "@crates//tokio-1.99.0:tokio-1.99.0")
    run_case("label with no <name>-<version> stem (a moved pin)", bumped, metadata, ["drift-set", "drift-unmapped"], "tokio-1.99.0")

    run_case("version attr off the workspace version", _in_package(text, "crates/ocx_mirror_report", 'version = "0.6.2"', 'version = "0.6.1"'),
             metadata, ["drift-version"], "0.6.1")
    run_case("version attr absent on a library", _in_package(text, "crates/ocx_mirror_report", '  version = "0.6.2",\n', ""),
             metadata, ["drift-version"], "None")

    mutated = _in_package(text, "crates/ocx_mirror_error", '"//crates/ocx_mirror_test_support:ocx_mirror_test_support", ', "")
    expect(mutated != text, "dev drop did not land")
    run_case("dev edge missing from the rust_test", mutated, metadata, ["drift-dev-set"], "['ocx_mirror_test_support']")
    both = json.loads(json.dumps(metadata))
    node = next(n for n in both["resolve"]["nodes"] if n["id"] == "member::ocx_mirror_report")
    next(d for d in node["deps"] if d["pkg"] == TOKIO)["dep_kinds"].append({"kind": "dev"})
    self_edge = '"//crates/ocx_mirror_report:ocx_mirror_report"'
    mutated = _in_package(text, "crates/ocx_mirror_report", f'{self_edge}, "{TOKIO}"', self_edge)
    expect(mutated != text, "test-side tokio drop did not land")
    run_case("dev edge that is also normal, twinned by the library edge", mutated, both, [])
    run_case("rust_test edge in neither normal nor dev", _in_package(text, "crates/ocx_mirror_error", f'"{TEMPFILE}", ', f'"{TEMPFILE}", "{OCX_OCI}", '),
             metadata, ["drift-dev-set"], "in BUILD not in Cargo: ['ocx_oci']")

    run_case("empty query output", "", metadata, ["drift-reader-floor", "drift-set"], "members with no rust rule")
    blocks = text.split("\n# " + ROOT)
    dropped = "\n# ".join([blocks[0]] + [ROOT + b for b in blocks[1:] if not b.startswith("/crates/ocx_mirror_source/")])
    expect(dropped.count("BUILD.bazel:9:13\n") < text.count("BUILD.bazel:9:13\n"), "package removal did not land")
    run_case("one package missing from the Bazel read", dropped, metadata, ["drift-reader-floor", "drift-set"], "//crates/ocx_mirror_source")
    few = json.loads(json.dumps(metadata))
    few["workspace_members"] = few["workspace_members"][:3]
    few_text = "\n# ".join([blocks[0]] + [ROOT + b for b in blocks[1:] if b.startswith(("/BUILD", "/crates/ocx_mirror_http/", "/crates/ocx_mirror_report/"))])
    floor = [f for f in check_drift(build_text=few_text, metadata=few, root=ROOT) if f.code == "drift-reader-floor"]
    expect(len(floor) == 1 and f"floor {MIN_MEMBERS}" in floor[0].message, f"3 Cargo members must red the floor: {floor}")
    print(f"RED  : fewer Cargo members than the floor — {floor[0].message}")
    checks += 1
    run_case("rule with no header", 'rust_library(\n  name = "x",\n  deps = [],\n)\n' + text, metadata, ["drift-orphan-rule"])

    print(f"bazel build drift self-test: {checks} checks passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="prove the gate red and green on synthetic readings")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    # `bazel` from PATH, as `ocx exec bazel --` leaves it.
    build_text, build_red = run(["bazel", "query", QUERY, "--output=build"])
    metadata_text, metadata_red = run(["cargo", "metadata", "--locked", "--format-version", "1"])
    findings = [finding for finding in (build_red, metadata_red) if finding]
    if findings:
        return report(findings)
    return report(check_drift(build_text=build_text, metadata=json.loads(metadata_text), root=str(REPO_ROOT)))


if __name__ == "__main__":
    raise SystemExit(main())
