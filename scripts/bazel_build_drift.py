#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`task bazel:build:drift` — every Cargo dep edge has a BUILD twin and back.

    scripts/bazel_build_drift.py --bazel '<bazel argv>'  # gate the live tree
    scripts/bazel_build_drift.py --self-test  # red/green proofs, no subprocess

Port of ocx's `scripts/bazel_build_drift.py` (adr_bazel_crate_split.md § C7).
Two readings, compared per workspace package:

* Bazel: `bazel query 'kind("rust_", //...)' --output=build` — the rules as
  loaded, after `all_crate_deps()` expanded. The BUILD text holds no
  third-party list at all, so a text parse would agree with itself forever.
* Cargo: `cargo metadata --locked` resolve graph — feature- and
  rename-resolved, the same lock crate_universe renders `@crates//` from.
  Run with the Bazel toolchain's cargo (rules_rust's upstream wrapper), so
  the gate needs no host Rust install and reads what Bazel builds with.

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
  Build-kind edges are excluded: no `cargo_build_script` exists here, and
  the next point keeps it that way until one does.

Two things the edge comparison cannot see, each its own finding:

* A first-party package with a non-empty `default` feature or a
  `custom-build` target (`build.rs`) reds: Bazel builds no feature a BUILD
  `crate_features` does not name and runs no build script without a
  `cargo_build_script`, so either compiles a different crate under Bazel.
  Mirror it into BUILD first, then widen this gate. Widened exactly once
  (ADMITTED_BUILD_SCRIPT): the root package's provenance `build.rs`, whose
  `__testing` output Bazel reads as `rustc_env_files` instead of running it
  (as ocx's graph does for `ocx_cli`) — admitted only while a root rule
  still names that file, so dropping the stand-in reds. The admission is by
  path only: what the script emits is not read, so a new `rustc-cfg` or
  link line in it would stay green here — keep that script to `rustc-env`.
* Every Cargo target that `cargo test` builds (`"test": true` — the lib and
  bin unit tests, each `tests/*.rs`) needs a `rust_test` in its package: a
  lib/bin one whose `crate` names a rule of the Cargo target's name, any
  other through `srcs` naming its source. Without it the case count never
  moves (the floor reads only targets that exist), so a new `tests/foo.rs`
  would run under nextest and nowhere under Bazel.

Feature variants (`<crate>_jsonschema`, `<crate>_jsonschema_test`) are
Bazel's twin of a non-default Cargo feature (VARIANT_FEATURES): the same
sources with `crate_features`, against the featured member crates. Their
edges stay out of the comparison above — an optional dependency (`schemars`)
is no edge of the default resolve — and one finding guards them instead:
every member whose Cargo `[features]` declares one needs its
`<crate>_<feature>` rule naming it in `crate_features`, or the gated code
would compile nowhere under Bazel.

Also: every `rust_library`/`rust_binary` carries `version` == the workspace
version, and a reader floor — every Cargo member must own >= 1 rust rule, at
least MIN_MEMBERS members must exist, and at least one member must have a
tested target — so an empty read is never green.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import re
import shlex
import subprocess
from pathlib import Path

from _gate import Finding, expect
from _gate import report as gate_report

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
CRATE_ATTR = re.compile(r'^  crate = "(?P<value>[^"]*)",$')
SRCS_ATTR = re.compile(r"^  srcs = (?P<value>.*)$")
#: Cargo target kinds a `rust_test` reaches through `crate =`; any other
#: tested kind (`test`, `example`, `bench`) through `srcs`.
CRATE_KINDS = frozenset({"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro", "bin"})
STRING_LITERAL = re.compile(r'"([^"]*)"')
DEP_LABEL_PREFIX = ("@crates//", "//")
VERSIONED_KINDS = ("rust_library", "rust_binary", "rust_proc_macro")
ENV_FILES_ATTR = re.compile(r"^  rustc_env_files = (?P<value>.*)$")
CRATE_FEATURES_ATTR = re.compile(r"^  crate_features = (?P<value>.*)$")
#: Non-default Cargo features Bazel builds as `<crate>_<feature>` variant rules.
VARIANT_FEATURES = ("jsonschema",)


def variant_feature(name: str | None) -> str | None:
    """`ocx_mirror_spec_jsonschema(_test)` -> `jsonschema`; a non-variant -> None."""
    for feature in VARIANT_FEATURES:
        if name and (name.endswith(f"_{feature}") or name.endswith(f"_{feature}_test")):
            return feature
    return None
#: (package, script, env file): the one build script Bazel stands in for.
ADMITTED_BUILD_SCRIPT = ("", "build.rs", "testing_provenance.env")
#: Root BUILD.bazel's hand-kept `source_scan`/`workspace_structure` data list.
MEMBER_SOURCES_ATTR = re.compile(r"_MEMBER_SOURCES = \[(?P<body>.*?)\]", re.DOTALL)
RUST_SOURCES_NAME = re.compile(r'name\s*=\s*"rust_sources"')


@dataclasses.dataclass
class BuildRead:
    normal: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    test: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    versions: list[tuple[str, str, str | None]] = dataclasses.field(default_factory=list)
    #: Per package: rule names `rust_test`s name as `crate`, and the
    #: package-relative paths they name as `srcs`.
    test_crates: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    test_srcs: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    #: Per package: files the rules name as `rustc_env_files`.
    env_files: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    #: Per package: `{variant rule name: its crate_features}`.
    variants: dict[str, dict[str, set[str]]] = dataclasses.field(default_factory=dict)
    targets: int = 0
    orphan_rules: int = 0


@dataclasses.dataclass
class CargoRead:
    normal: dict[str, set[str]]
    dev: dict[str, set[str]]
    own_name: dict[str, str]
    stems: dict[str, str]
    version: str
    #: Per package: `(kinds, name, package-relative src_path)` of every
    #: target with `"test": true`.
    tested: dict[str, list[tuple[frozenset[str], str, str]]] = dataclasses.field(default_factory=dict)
    default_features: dict[str, list[str]] = dataclasses.field(default_factory=dict)
    build_scripts: dict[str, list[str]] = dataclasses.field(default_factory=dict)
    #: Per package: every feature its `[features]` table declares.
    features: dict[str, set[str]] = dataclasses.field(default_factory=dict)


def pkg_label(package: str) -> str:
    return f"//{package}"


def label_target(label: str) -> str:
    """`//pkg:name` -> `name`; `//pkg/name` -> `name`."""
    return label.split(":", 1)[1] if ":" in label else label.rsplit("/", 1)[-1]


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
            if variant_feature(name):
                read.variants.setdefault(package, {})[name] = set()
        elif match := VERSION_ATTR.match(line):
            version = match.group("value")
        elif variant_feature(name):
            # Name precedes every other attribute in `--output=build`.
            if match := CRATE_FEATURES_ATTR.match(line):
                read.variants[package][name].update(STRING_LITERAL.findall(match.group("value")))
        elif kind == "rust_test" and (match := CRATE_ATTR.match(line)):
            read.test_crates.setdefault(package, set()).add(label_target(match.group("value")))
        elif kind == "rust_test" and (match := SRCS_ATTR.match(line)):
            srcs = read.test_srcs.setdefault(package, set())
            srcs.update(label_target(literal) for literal in STRING_LITERAL.findall(match.group("value")))
        elif kind in VERSIONED_KINDS and (match := ENV_FILES_ATTR.match(line)):
            files = read.env_files.setdefault(package, set())
            files.update(label_target(literal) for literal in STRING_LITERAL.findall(match.group("value")))
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
    tested: dict[str, list[tuple[frozenset[str], str, str]]] = {}
    default_features: dict[str, list[str]] = {}
    build_scripts: dict[str, list[str]] = {}
    features: dict[str, set[str]] = {}
    version = ""
    for member in document["workspace_members"]:
        package = by_id[member]
        directory = str(Path(package["manifest_path"]).parent)
        key = "" if directory == root else directory[len(root) + 1 :]
        own_name[key] = package["name"]
        if default := package.get("features", {}).get("default"):
            default_features[key] = default
        features[key] = set(package.get("features", {}))
        for target in package.get("targets", []):
            kinds = frozenset(target["kind"])
            source = Path(target["src_path"]).relative_to(directory, walk_up=True).as_posix()
            if "custom-build" in kinds:
                build_scripts.setdefault(key, []).append(source)
            elif target.get("test"):
                tested.setdefault(key, []).append((kinds, target["name"], source))
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
    return CargoRead(normal=normal, dev=dev, own_name=own_name, stems=stems, version=version,
                     tested=tested, default_features=default_features, build_scripts=build_scripts, features=features)


def resolve(label: str, cargo: CargoRead) -> str | None:
    """Bazel label -> Cargo package name; None when nothing matches."""
    if label.startswith("@crates//"):
        return cargo.stems.get(label[len("@crates//") :].split(":", 1)[0])
    target = label_target(label)
    return target if target in cargo.own_name.values() else None


def check_member_sources(*, root_build_text: str, crate_names: list[str], crate_build_texts: dict[str, str | None]) -> list[Finding]:
    """Root `_MEMBER_SOURCES` == one `//crates/<d>:rust_sources` per `crates/<d>/Cargo.toml`.

    A crate absent from the list is silently skipped by `//:source_scan` and
    `//:workspace_structure` (`source_scan.rs`, `workspace_structure.rs`) —
    their data comes from this list, not a glob.
    """
    findings: list[Finding] = []
    match = MEMBER_SOURCES_ATTR.search(root_build_text)
    listed = set(STRING_LITERAL.findall(match.group("body"))) if match else set()
    wanted = {f"//crates/{name}:rust_sources" for name in crate_names}
    missing, extra = sorted(wanted - listed), sorted(listed - wanted)
    if missing or extra:
        findings.append(
            Finding(
                "drift-member-sources",
                f"BUILD drift: root BUILD.bazel _MEMBER_SOURCES — crates/*/Cargo.toml with no listed label: {missing}; listed labels with no matching crate: {extra}",
            )
        )
    for name in sorted(crate_names):
        text = crate_build_texts.get(name)
        if text is None:
            findings.append(Finding("drift-member-sources", f"BUILD drift: crates/{name} has no BUILD.bazel"))
        elif not RUST_SOURCES_NAME.search(text):
            findings.append(Finding("drift-member-sources", f"BUILD drift: crates/{name}/BUILD.bazel defines no rust_sources target"))
    return findings


def read_member_sources_inputs(root: Path) -> tuple[str, list[str], dict[str, str | None]]:
    root_build_text = (root / "BUILD.bazel").read_text(encoding="utf-8")
    crate_names = sorted(p.parent.name for p in (root / "crates").glob("*/Cargo.toml"))
    crate_build_texts: dict[str, str | None] = {}
    for name in crate_names:
        build_path = root / "crates" / name / "BUILD.bazel"
        crate_build_texts[name] = build_path.read_text(encoding="utf-8") if build_path.exists() else None
    return root_build_text, crate_names, crate_build_texts


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

    for package, default in sorted(cargo.default_features.items()):
        findings.append(
            Finding("drift-default-features", f"BUILD drift: {pkg_label(package)} Cargo `default` feature enables {default} — Bazel builds none of it; mirror it into the BUILD rules' `crate_features` first")
        )
    for package, declared_features in sorted(cargo.features.items()):
        for feature in sorted(declared_features & set(VARIANT_FEATURES)):
            rule = f"{cargo.own_name[package]}_{feature}"
            if feature not in read.variants.get(package, {}).get(rule, set()):
                findings.append(
                    Finding("drift-feature-variant", f"BUILD drift: {pkg_label(package)} Cargo feature `{feature}` has no `{rule}` rule with `crate_features = [\"{feature}\"]` — its gated code compiles nowhere under Bazel")
                )
    admitted_package, admitted_script, stand_in = ADMITTED_BUILD_SCRIPT
    for package, scripts in sorted(cargo.build_scripts.items()):
        if (package, scripts) == (admitted_package, [admitted_script]):
            if stand_in in read.env_files.get(package, set()):
                continue
            findings.append(
                Finding("drift-build-script", f"BUILD drift: {pkg_label(package)} {scripts} is admitted only while a rule in the package names `rustc_env_files = [\"{stand_in}\"]` — without it Bazel builds the crate with no provenance at all")
            )
            continue
        findings.append(
            Finding("drift-build-script", f"BUILD drift: {pkg_label(package)} has a Cargo build script {scripts} — Bazel runs none; mirror it into a `cargo_build_script` first")
        )
    if not cargo.tested:
        findings.append(
            Finding("drift-test-floor", "BUILD drift: no workspace member has a Cargo target with `\"test\": true` — the tested-target check read nothing and would pass vacuously; is `cargo metadata` output missing `targets`?")
        )
    for package, targets in sorted(cargo.tested.items()):
        for kinds, name, source in targets:
            if kinds & CRATE_KINDS:
                covered, how = name in read.test_crates.get(package, set()), f'`crate = ":{name}"`'
            else:
                covered, how = source in read.test_srcs.get(package, set()), f'`srcs = ["{source}"]`'
            if not covered:
                findings.append(
                    Finding("drift-test-target", f"BUILD drift: {pkg_label(package)} Cargo {'/'.join(sorted(kinds))} target {name} ({source}) is tested by cargo, but no rust_test in the package has {how} — it would run under nextest and nowhere under Bazel")
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
    if not findings:
        print("bazel:build:drift: every Cargo edge has a BUILD twin and back; version attrs match; every tested Cargo target has a rust_test")
    return gate_report(findings)


# ---------------------------------------------------------------------------
# Self-test — synthetic readings in the grammar bazel 9.2.0 prints, no subprocess.
# ---------------------------------------------------------------------------

ROOT = "/abs/ws"
TOKIO = "@crates//tokio-1.52.3:tokio-1.52.3"
SERDE = "@crates//serde-1.0.228:serde-1.0.228"
TOML = "@crates//toml-1.1.4+spec-1.1.0:toml-1.1.4+spec-1.1.0"
OCX_OCI = "@crates//ocx_oci-0.6.2:ocx_oci-0.6.2"
TEMPFILE = "@crates//tempfile-3.27.0:tempfile-3.27.0"
SCHEMARS = "@crates__schemars-1.2.1//:schemars"
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


def _rule(package: str, kind: str, name: str, deps: list[str], version: str | None = "0.6.2", extra: tuple[str, ...] = ()) -> str:
    lines = [f"# {ROOT}/{package + '/' if package else ''}BUILD.bazel:9:13", f"{kind}(", f'  name = "{name}",', *extra]
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
        if index % 2 == 0:
            # The members declaring `jsonschema` below carry its variant pair;
            # the optional schemars edge and the variant-to-variant edge must
            # stay out of the edge comparison.
            featured = (f'  crate_features = ["jsonschema"],',)
            records.append(_rule(package, "rust_library", f"{name}_jsonschema", labels + [SCHEMARS, f"//{package}:{name}_jsonschema"], extra=featured))
            records.append(_rule(package, "rust_test", f"{name}_jsonschema_test", [TEMPFILE], version=None,
                                 extra=(f'  crate = "//{package}:{name}_jsonschema",', *featured)))
        dev = []
        if index != 7:
            dev = [TEMPFILE, "//crates/ocx_mirror_test_support:ocx_mirror_test_support"]
            # A test target also names its own lib (the self-edge) and a normal edge.
            records.append(_rule(package, "rust_test", f"{name}_test", dev + [f"//{package}:{name}", TOKIO], version=None,
                                 extra=(f'  crate = "//{package}:{name}",',)))
        directory = f"{ROOT}/{package}" if package else ROOT
        # test_support's lib is `test = false` (index 7): exempt, no rust_test.
        targets = [{"kind": ["lib"], "name": name, "src_path": f"{directory}/src/lib.rs", "test": index != 7}]
        if package == "":
            records.append(_rule("", "rust_binary", "ocx-mirror", normal + ["//:ocx_mirror"]))
            records.append(_rule("", "rust_test", "ocx_mirror_bin_test", dev + [TOKIO], version=None, extra=('  crate = "//:ocx-mirror",',)))
            records.append(_rule("", "rust_test", "log_targets", dev + ["//:ocx_mirror", TOKIO], version=None,
                                 extra=('  srcs = ["//:tests/log_targets.rs"],',)))
            targets += [{"kind": ["bin"], "name": "ocx-mirror", "src_path": f"{ROOT}/src/main.rs", "test": True},
                        {"kind": ["test"], "name": "log_targets", "src_path": f"{ROOT}/tests/log_targets.rs", "test": True},
                        {"kind": ["example"], "name": "demo", "src_path": f"{ROOT}/examples/demo.rs", "test": False}]
        member_id = f"member::{name}"
        members.append(member_id)
        packages.append({"id": member_id, "name": name, "version": "0.6.2",
                         "manifest_path": f"{directory}/Cargo.toml", "targets": targets,
                         "features": {"default": [], "jsonschema": []} if index % 2 == 0 else {}})
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

    run_case("empty query output", "", metadata, ["drift-feature-variant", "drift-reader-floor", "drift-set", "drift-test-target"], "members with no rust rule")
    blocks = text.split("\n# " + ROOT)
    dropped = "\n# ".join([blocks[0]] + [ROOT + b for b in blocks[1:] if not b.startswith("/crates/ocx_mirror_source/")])
    expect(dropped.count("BUILD.bazel:9:13\n") < text.count("BUILD.bazel:9:13\n"), "package removal did not land")
    run_case("one package missing from the Bazel read", dropped, metadata, ["drift-reader-floor", "drift-set", "drift-test-target"], "//crates/ocx_mirror_source")
    few = json.loads(json.dumps(metadata))
    few["workspace_members"] = few["workspace_members"][:3]
    few_text = "\n# ".join([blocks[0]] + [ROOT + b for b in blocks[1:] if b.startswith(("/BUILD", "/crates/ocx_mirror_http/", "/crates/ocx_mirror_report/"))])
    floor = [f for f in check_drift(build_text=few_text, metadata=few, root=ROOT) if f.code == "drift-reader-floor"]
    expect(len(floor) == 1 and f"floor {MIN_MEMBERS}" in floor[0].message, f"3 Cargo members must red the floor: {floor}")
    print(f"RED  : fewer Cargo members than the floor — {floor[0].message}")
    checks += 1
    def member(meta: dict, name: str) -> dict:
        return next(p for p in meta["packages"] if p["id"] == f"member::{name}")

    added = json.loads(json.dumps(metadata))
    member(added, "ocx_mirror")["targets"].append({"kind": ["test"], "name": "foo", "src_path": f"{ROOT}/tests/foo.rs", "test": True})
    run_case("new tests/foo.rs with no rust_test", text, added, ["drift-test-target"], "tests/foo.rs")
    added = json.loads(json.dumps(metadata))
    member(added, "ocx_mirror_spec")["targets"].append({"kind": ["test"], "name": "api", "src_path": f"{ROOT}/crates/ocx_mirror_spec/tests/api.rs", "test": True})
    run_case("a crate's first tests/ file with no rust_test", text, added, ["drift-test-target"], "//crates/ocx_mirror_spec Cargo test target api (tests/api.rs)")
    mutated = _in_package(text, "crates/ocx_mirror_spec", '  crate = "//crates/ocx_mirror_spec:ocx_mirror_spec",\n', "")
    expect(mutated != text, "lib crate drop did not land")
    run_case("lib unit tests with no rust_test naming the lib", mutated, metadata, ["drift-test-target"], "lib target ocx_mirror_spec")
    mutated = text.replace('  crate = "//:ocx-mirror",\n', "")
    expect(mutated != text, "bin crate drop did not land")
    run_case("bin unit tests with no rust_test naming the binary", mutated, metadata, ["drift-test-target"], "bin target ocx-mirror")
    featured = json.loads(json.dumps(metadata))
    member(featured, "ocx_mirror_spec")["features"]["default"] = ["jsonschema"]
    run_case("first-party non-empty `default` feature", text, featured, ["drift-default-features"], "crate_features")
    built = json.loads(json.dumps(metadata))
    member(built, "ocx_mirror_http")["targets"].append({"kind": ["custom-build"], "name": "build-script-build", "src_path": f"{ROOT}/crates/ocx_mirror_http/build.rs", "test": False})
    run_case("first-party build.rs", text, built, ["drift-build-script"], "cargo_build_script")
    unfeatured = _in_package(text, "crates/ocx_mirror_report", '  crate_features = ["jsonschema"],\n  deps', "  deps")
    expect(unfeatured != text, "variant crate_features drop did not land")
    run_case("feature variant rule without its crate_features", unfeatured, metadata, ["drift-feature-variant"], "ocx_mirror_report_jsonschema")
    unvaried = _in_package(text, "crates/ocx_mirror_error", 'name = "ocx_mirror_error_jsonschema"', 'name = "ocx_mirror_errors_jsonschema"')
    expect(unvaried != text, "variant rename did not land")
    run_case("Cargo feature whose variant rule is misnamed", unvaried, metadata, ["drift-feature-variant"], "//crates/ocx_mirror_error Cargo feature `jsonschema`")
    rooted = json.loads(json.dumps(metadata))
    member(rooted, "ocx_mirror")["targets"].append({"kind": ["custom-build"], "name": "build-script-build", "src_path": f"{ROOT}/build.rs", "test": False})
    # The root library is the first record, ahead of any split point `_in_package` uses.
    stand_in = text.replace('  name = "ocx_mirror",\n', '  name = "ocx_mirror",\n  rustc_env_files = ["//:testing_provenance.env"],\n', 1)
    expect(stand_in.count("rustc_env_files") == 1, "stand-in insert did not land")
    run_case("root build.rs admitted: a root rule names the stand-in env file", stand_in, rooted, [])
    run_case("root build.rs without the rustc_env_files stand-in", text, rooted, ["drift-build-script"], "rustc_env_files")
    twice = json.loads(json.dumps(rooted))
    member(twice, "ocx_mirror")["targets"].append({"kind": ["custom-build"], "name": "build-script-build", "src_path": f"{ROOT}/build/extra.rs", "test": False})
    run_case("a second root build script next to the admitted one", stand_in, twice, ["drift-build-script"], "cargo_build_script")
    elsewhere = json.loads(json.dumps(built))
    run_case("a member build.rs even with the root stand-in present", stand_in, elsewhere, ["drift-build-script"], "ocx_mirror_http")

    untargeted = json.loads(json.dumps(metadata))
    for package in untargeted["packages"]:
        package.pop("targets", None)
    run_case("cargo metadata yields no tested target at all", text, untargeted, ["drift-test-floor"], "no workspace member")

    run_case("rule with no header", 'rust_library(\n  name = "x",\n  deps = [],\n)\n' + text, metadata, ["drift-orphan-rule"])

    def run_member_case(label: str, root_text: str, names: list[str], build_texts: dict[str, str | None], want: list[str], needle: str = "") -> None:
        nonlocal checks
        got = check_member_sources(root_build_text=root_text, crate_names=names, crate_build_texts=build_texts)
        codes = sorted({f.code for f in got})
        expect(codes == want, f"{label}: expected {want}, got {codes}: {[f.message for f in got]}")
        if needle:
            expect(any(needle in f.message for f in got), f"{label}: no finding names {needle!r}: {[f.message for f in got]}")
        print(f"{'GREEN' if not want else 'RED  '}: {label}" + (f" — {got[0].message}" if got else ""))
        checks += 1

    member_names = sorted(name for _, name in MEMBERS if name != "ocx_mirror")
    member_root = "_MEMBER_SOURCES = [\n" + "".join(f'    "//crates/{n}:rust_sources",\n' for n in member_names) + "]\n"
    member_builds = {n: f'filegroup(\n    name = "rust_sources",\n    srcs = glob(["**/*.rs"]) + ["Cargo.toml"],\n)\n' for n in member_names}
    run_member_case("agreeing member-sources tree (every crate listed, every BUILD defines rust_sources)", member_root, member_names, member_builds, [])

    missing_root = member_root.replace(f'    "//crates/{member_names[0]}:rust_sources",\n', "")
    expect(missing_root != member_root, "member drop did not land")
    run_member_case("a new crates/<d>/Cargo.toml absent from _MEMBER_SOURCES", missing_root, member_names, member_builds, ["drift-member-sources"], f"//crates/{member_names[0]}:rust_sources")

    stale_names = member_names[1:]
    run_member_case("a stale label in _MEMBER_SOURCES for a crate that no longer exists", member_root, stale_names, {n: v for n, v in member_builds.items() if n != member_names[0]}, ["drift-member-sources"], f"//crates/{member_names[0]}:rust_sources")

    unbuilt = dict(member_builds)
    unbuilt[member_names[0]] = None
    run_member_case("a listed crate with no BUILD.bazel at all", member_root, member_names, unbuilt, ["drift-member-sources"], f"crates/{member_names[0]} has no BUILD.bazel")

    untargeted = dict(member_builds)
    untargeted[member_names[0]] = 'rust_library(\n    name = "%s",\n)\n' % member_names[0]
    run_member_case("a listed crate whose BUILD.bazel defines no rust_sources target", member_root, member_names, untargeted, ["drift-member-sources"], f"crates/{member_names[0]}/BUILD.bazel defines no rust_sources")

    print(f"bazel build drift self-test: {checks} checks passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="prove the gate red and green on synthetic readings")
    parser.add_argument("--bazel", default="bazel", help="the Bazel command, shell-quoted (the task passes its `{{.BAZEL}}`)")
    parser.add_argument(
        "--run-flags",
        default="",
        help="build options for the cargo wrapper's `bazel run`, shell-quoted — CI's main-push lane passes its upload grant, so the wrapper it compiles reaches the shared cache",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    root_build_text, crate_names, crate_build_texts = read_member_sources_inputs(REPO_ROOT)
    findings = check_member_sources(root_build_text=root_build_text, crate_names=crate_names, crate_build_texts=crate_build_texts)
    bazel = shlex.split(args.bazel)
    build_text, build_red = run([*bazel, "query", QUERY, "--output=build"])
    # The wrapper runs cargo in the client's working directory (REPO_ROOT)
    # with the toolchain's rustc first on PATH; Bazel's own output is stderr.
    metadata_text, metadata_red = run([*bazel, "run", *shlex.split(args.run_flags), "@rules_rust//tools/upstream_wrapper:cargo", "--",
                                       "metadata", "--locked", "--format-version", "1"])
    findings += [finding for finding in (build_red, metadata_red) if finding]
    if build_red or metadata_red:
        return report(findings)
    return report(findings + check_drift(build_text=build_text, metadata=json.loads(metadata_text), root=str(REPO_ROOT)))


if __name__ == "__main__":
    raise SystemExit(main())
