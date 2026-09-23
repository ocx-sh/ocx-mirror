// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The crate map, enforced: `crates/crate_map.toml` says which workspace
//! member may depend on which, and this test fails — naming the offender — on
//! any edge, member or source line that breaks it
//! (adr_bazel_crate_split.md § C1 "Enforcement", plan C-002 (a)–(g); (h) guards ADR § C2's byte-identical `User-Agent`).
//!
//! `check` and `scan_sources` are pure: the real workspace goes through them
//! once (`real_workspace_has_no_violations`), and every clause has a
//! red-proof that plants one violation into a clean synthetic workspace and
//! asserts it is reported.
//!
//! Planted source lines are assembled with `concat!` on purpose: ocx's own
//! `satellite:verify` scans this file line by line (`tests/*.rs`), and a
//! literal forbidden path here would fail it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use serde::Deserialize;

// ── Inputs ──

/// The subset of `cargo metadata --format-version 1` the checker reads.
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    /// The package name — a `package = "…"` rename still reports the real crate.
    name: String,
    /// `None` = normal, `Some("dev")`, `Some("build")`.
    kind: Option<String>,
}

#[derive(Deserialize)]
struct CrateMap {
    allowed: BTreeMap<String, Vec<String>>,
    ocx: OcxTable,
    dev: DevTable,
}

#[derive(Deserialize)]
struct OcxTable {
    allowed: Vec<String>,
}

#[derive(Deserialize)]
struct DevTable {
    allowed_everywhere: Vec<String>,
}

/// One workspace member as the checker sees it.
struct Member {
    name: String,
    /// The member's `Cargo.toml` carries `[lints] workspace = true`.
    inherits_lints: bool,
    /// The member's `Cargo.toml` carries `version.workspace = true`.
    inherits_version: bool,
    dependencies: Vec<Dependency>,
}

#[derive(Debug, PartialEq)]
struct Violation {
    clause: char,
    message: String,
}

fn violation(clause: char, message: String) -> Violation {
    Violation { clause, message }
}

/// Builds the member list from `cargo metadata` JSON; `read_manifest` returns
/// a member's `Cargo.toml` text by path.
fn members(metadata_json: &str, read_manifest: impl Fn(&Path) -> String) -> Vec<Member> {
    let metadata: Metadata = serde_json::from_str(metadata_json).expect("cargo metadata JSON parses");
    let ids: BTreeSet<&str> = metadata.workspace_members.iter().map(String::as_str).collect();
    metadata
        .packages
        .into_iter()
        .filter(|package| ids.contains(package.id.as_str()))
        .map(|package| {
            let manifest: toml::Table =
                toml::from_str(&read_manifest(&package.manifest_path)).expect("member Cargo.toml parses");
            Member {
                inherits_lints: inherits_workspace(&manifest, "lints", None),
                inherits_version: inherits_workspace(&manifest, "package", Some("version")),
                name: package.name,
                dependencies: package.dependencies,
            }
        })
        .collect()
}

/// `[table] workspace = true`, or `[table] key.workspace = true` with `key`.
fn inherits_workspace(manifest: &toml::Table, table: &str, key: Option<&str>) -> bool {
    let Some(mut value) = manifest.get(table) else {
        return false;
    };
    if let Some(key) = key {
        let Some(inner) = value.get(key) else { return false };
        value = inner;
    }
    value.get("workspace").and_then(toml::Value::as_bool) == Some(true)
}

// ── Clauses (a)–(f), (h) ──

fn check(members: &[Member], map: &CrateMap) -> Vec<Violation> {
    let mut violations = Vec::new();
    let names: BTreeSet<&str> = members.iter().map(|member| member.name.as_str()).collect();

    // (d) every member has a row, every row has a member.
    for name in &names {
        if !map.allowed.contains_key(*name) {
            violations.push(violation(
                'd',
                format!("member {name} has no [allowed] row in crates/crate_map.toml"),
            ));
        }
    }
    for row in map.allowed.keys() {
        if !names.contains(row.as_str()) {
            violations.push(violation('d', format!("[allowed] row {row} names no workspace member")));
        }
    }

    for member in members {
        let from = &member.name;
        // (e) lint policy inherited.
        if !member.inherits_lints {
            violations.push(violation(
                'e',
                format!("member {from} lacks `[lints] workspace = true`"),
            ));
        }
        // (h) `CARGO_PKG_VERSION` is the workspace version in every member
        // (ADR § C2): `ocx_mirror_source`'s `User-Agent` reads it and must stay
        // byte-identical to the pre-split binary's.
        if !member.inherits_version {
            violations.push(violation(
                'h',
                format!("member {from} lacks `version.workspace = true`"),
            ));
        }
        // (f) mirror-owned members are `ocx_mirror_*`.
        if from != "ocx_mirror" && !from.starts_with("ocx_mirror_") {
            violations.push(violation('f', format!("member {from} is not named ocx_mirror_*")));
        }

        let allowed = map.allowed.get(from);
        for dependency in &member.dependencies {
            let to = &dependency.name;
            if names.contains(to.as_str()) {
                // A member without a row is already reported by (d).
                let Some(allowed) = allowed else { continue };
                let permitted = allowed.contains(to);
                match dependency.kind.as_deref() {
                    // (b) dev edges may also reach the dev-only helpers.
                    Some("dev") => {
                        if !permitted && !map.dev.allowed_everywhere.contains(to) {
                            violations.push(violation(
                                'b',
                                format!("dev edge {from} -> {to} is not in crates/crate_map.toml"),
                            ));
                        }
                    }
                    // (a) normal and build edges.
                    kind => {
                        if !permitted {
                            let kind = kind.unwrap_or("normal");
                            violations.push(violation(
                                'a',
                                format!("{kind} edge {from} -> {to} is not in crates/crate_map.toml"),
                            ));
                        }
                    }
                }
            } else if is_ocx_crate(to) && !map.ocx.allowed.contains(to) {
                // (c) the satellite linking rule, any dependency kind.
                violations.push(violation(
                    'c',
                    format!("member {from} depends on {to}, which is not in crates/crate_map.toml [ocx]"),
                ));
            }
        }
    }
    violations
}

/// An ocx-owned package name. ocx's CLI package is named plain `ocx`
/// (`external/ocx/crates/ocx_cli/Cargo.toml`) and its shim `ocx-shim`, so a
/// bare `ocx_` prefix check would let the one crate the satellite rule names
/// first slip through.
fn is_ocx_crate(name: &str) -> bool {
    name == "ocx" || name.starts_with("ocx_") || name.starts_with("ocx-")
}

// ── Clause (g): source scan ──

/// ocx's satellite source regex, verbatim from
/// `external/ocx/taskfiles/satellite.taskfile.yml` (the `names`/`src_re`
/// assignments, ~lines 171-172). POSIX bracket classes are valid in `regex`.
fn satellite_source_regex() -> Regex {
    let names = "(store|shell|project|package_manager|setup|announce|script|test_support|cli)";
    let src_re = format!(
        "(^|[^:[:alnum:]_])(::)?ocx_{names}::|(^|[^[:alnum:]_])(use|extern[[:space:]]+crate)[[:space:]]+(::)?ocx_{names}([^[:alnum:]_]|$)"
    );
    Regex::new(&src_re).expect("satellite regex compiles")
}

/// Line-wise like ocx's awk: strip from the first `//`, then match.
fn scan_sources(files: &[(PathBuf, String)]) -> Vec<Violation> {
    let regex = satellite_source_regex();
    let mut violations = Vec::new();
    for (path, text) in files {
        for (index, line) in text.lines().enumerate() {
            let code = line.find("//").map_or(line, |comment| &line[..comment]);
            if regex.is_match(code) {
                violations.push(violation('g', format!("{}:{}: {line}", path.display(), index + 1)));
            }
        }
    }
    violations
}

fn rust_files(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).expect("source directory is readable") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let text = std::fs::read_to_string(&path).expect("source file is readable");
            out.push((path, text));
        }
    }
}

// ── The real workspace ──

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn crate_map(text: &str) -> CrateMap {
    toml::from_str(text).expect("crates/crate_map.toml parses")
}

#[test]
fn real_workspace_has_no_violations() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(root())
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = String::from_utf8(output.stdout).expect("cargo metadata output is UTF-8");

    let members = members(&json, |path| {
        std::fs::read_to_string(path).expect("member Cargo.toml is readable")
    });
    let map = crate_map(&std::fs::read_to_string(root().join("crates/crate_map.toml")).expect("crate map is readable"));
    let mut violations = check(&members, &map);

    // Same roots as ocx's scan (`src/*.rs`, `crates/*.rs`, `tests/*.rs` git
    // pathspecs, where `*` crosses `/`); each must list at least one file or
    // the negative would hold vacuously.
    let mut files = Vec::new();
    for dir in ["src", "crates", "tests"] {
        let before = files.len();
        rust_files(&root().join(dir), &mut files);
        assert!(
            files.len() > before,
            "{dir}/ holds no .rs file — the scan no longer matches the layout"
        );
    }
    // Deterministic report order, independent of `read_dir`.
    files.sort_by(|a, b| a.0.cmp(&b.0));
    violations.extend(scan_sources(&files));

    assert!(violations.is_empty(), "crate map violations:\n{violations:#?}");
}

// ── Red-proofs: one planted violation per clause ──

/// A clean two-member workspace mirroring today's real one.
const MAP: &str = r#"
[allowed]
ocx_mirror = ["ocx_mirror_source"]
ocx_mirror_source = []

[ocx]
allowed = ["ocx_oci", "ocx_package", "ocx_python"]

[dev]
allowed_everywhere = ["ocx_mirror_test_support"]
"#;

/// A member manifest satisfying (e) and (h).
const MANIFEST_OK: &str = "[package]\nname = \"x\"\nversion.workspace = true\n\n[lints]\nworkspace = true\n";

/// A synthetic member: `(name, [(dependency, kind)])`.
type SyntheticPackage = (&'static str, &'static [(&'static str, Option<&'static str>)]);

/// Cargo metadata JSON over the given members.
fn metadata_json(packages: &[SyntheticPackage]) -> String {
    let packages: Vec<serde_json::Value> = packages
        .iter()
        .map(|(name, dependencies)| {
            let dependencies: Vec<serde_json::Value> = dependencies
                .iter()
                .map(|(dependency, kind)| serde_json::json!({ "name": dependency, "kind": kind }))
                .collect();
            serde_json::json!({
                "id": format!("path+file:///ws#{name}@0.6.2"),
                "name": name,
                "manifest_path": format!("/ws/{name}/Cargo.toml"),
                "dependencies": dependencies,
            })
        })
        .collect();
    let ids: Vec<&str> = packages
        .iter()
        .map(|package| package["id"].as_str().expect("id"))
        .collect();
    serde_json::json!({ "packages": packages, "workspace_members": ids }).to_string()
}

fn clean_packages() -> Vec<SyntheticPackage> {
    vec![
        (
            "ocx_mirror",
            &[
                ("ocx_mirror_source", None),
                ("ocx_oci", None),
                ("serde", None),
                ("toml", Some("dev")),
            ],
        ),
        (
            "ocx_mirror_source",
            &[("ocx_package", None), ("ocx_python", None), ("serde", None)],
        ),
    ]
}

/// Runs the checker; asserts exactly one violation of `clause` naming every
/// string in `names`.
fn assert_single(
    packages: &[SyntheticPackage],
    map: &str,
    lints: impl Fn(&Path) -> String,
    clause: char,
    names: &[&str],
) {
    let violations = check(&members(&metadata_json(packages), lints), &crate_map(map));
    assert_eq!(
        violations.len(),
        1,
        "expected one ({clause}) violation, got {violations:#?}"
    );
    assert_eq!(violations[0].clause, clause, "{violations:#?}");
    for name in names {
        assert!(
            violations[0].message.contains(name),
            "{:?} does not name {name}",
            violations[0].message
        );
    }
}

#[test]
fn clean_synthetic_workspace_has_no_violations() {
    let violations = check(
        &members(&metadata_json(&clean_packages()), |_| MANIFEST_OK.into()),
        &crate_map(MAP),
    );
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn red_proof_a_upward_normal_edge() {
    let mut packages = clean_packages();
    packages[1] = ("ocx_mirror_source", &[("ocx_mirror", None)]);
    assert_single(
        &packages,
        MAP,
        |_| MANIFEST_OK.into(),
        'a',
        &["ocx_mirror_source -> ocx_mirror", "normal"],
    );
}

#[test]
fn red_proof_a_build_edge_onto_dev_only_crate() {
    let map = MAP.replace(
        "ocx_mirror_source = []",
        "ocx_mirror_source = []\nocx_mirror_test_support = []",
    );
    let mut packages = clean_packages();
    packages[1] = ("ocx_mirror_source", &[("ocx_mirror_test_support", Some("build"))]);
    packages.push(("ocx_mirror_test_support", &[]));
    assert_single(
        &packages,
        &map,
        |_| MANIFEST_OK.into(),
        'a',
        &["build edge ocx_mirror_source -> ocx_mirror_test_support"],
    );
}

#[test]
fn red_proof_b_dev_edge_outside_the_map() {
    let mut packages = clean_packages();
    packages[1] = (
        "ocx_mirror_source",
        &[("ocx_mirror", Some("dev")), ("ocx_mirror_test_support", Some("dev"))],
    );
    // The test-support dev edge is allowed everywhere; the ocx_mirror one is not.
    let map = MAP.replace(
        "ocx_mirror_source = []",
        "ocx_mirror_source = []\nocx_mirror_test_support = []",
    );
    packages.push(("ocx_mirror_test_support", &[]));
    assert_single(
        &packages,
        &map,
        |_| MANIFEST_OK.into(),
        'b',
        &["dev edge ocx_mirror_source -> ocx_mirror is not"],
    );
}

#[test]
fn red_proof_c_forbidden_ocx_crates() {
    let cases: [(&str, SyntheticPackage); 4] = [
        // cargo metadata names ocx's CLI package `ocx`, never `ocx_cli`.
        ("ocx", ("ocx_mirror", &[("ocx_mirror_source", None), ("ocx", None)])),
        (
            "ocx-shim",
            ("ocx_mirror", &[("ocx_mirror_source", None), ("ocx-shim", None)]),
        ),
        (
            "ocx_store",
            (
                "ocx_mirror",
                &[("ocx_mirror_source", None), ("ocx_store", Some("build"))],
            ),
        ),
        (
            "ocx_test_support",
            (
                "ocx_mirror",
                &[("ocx_mirror_source", None), ("ocx_test_support", Some("dev"))],
            ),
        ),
    ];
    for (forbidden, member) in cases {
        let mut packages = clean_packages();
        packages[0] = member;
        assert_single(&packages, MAP, |_| MANIFEST_OK.into(), 'c', &["ocx_mirror", forbidden]);
    }
}

#[test]
fn red_proof_d_member_without_row() {
    let mut packages = clean_packages();
    packages.push(("ocx_mirror_http", &[]));
    assert_single(&packages, MAP, |_| MANIFEST_OK.into(), 'd', &["ocx_mirror_http"]);
}

#[test]
fn red_proof_d_row_without_member() {
    let map = MAP.replace("ocx_mirror_source = []", "ocx_mirror_source = []\nocx_mirror_gone = []");
    assert_single(
        &clean_packages(),
        &map,
        |_| MANIFEST_OK.into(),
        'd',
        &["ocx_mirror_gone"],
    );
}

#[test]
fn red_proof_e_member_without_workspace_lints() {
    let lints = |path: &Path| {
        if path.starts_with("/ws/ocx_mirror_source") {
            "[package]\nname = \"ocx_mirror_source\"\nversion.workspace = true\n\n[lints.rust]\nwarnings = \"deny\"\n"
                .to_owned()
        } else {
            MANIFEST_OK.to_owned()
        }
    };
    assert_single(&clean_packages(), MAP, lints, 'e', &["ocx_mirror_source"]);
}

#[test]
fn red_proof_h_member_with_its_own_version() {
    // Only the root pins a literal version, so exactly the root is reported.
    let manifest = |path: &Path| {
        if path.starts_with("/ws/ocx_mirror") {
            "[package]\nname = \"x\"\nversion = \"0.6.2\"\n\n[lints]\nworkspace = true\n".to_owned()
        } else {
            MANIFEST_OK.to_owned()
        }
    };
    assert_single(
        &clean_packages(),
        MAP,
        manifest,
        'h',
        &["member ocx_mirror lacks `version.workspace = true`"],
    );
}

#[test]
fn red_proof_f_member_outside_the_prefix() {
    let map = MAP.replace("ocx_mirror_source = []", "ocx_mirror_source = []\nmirror_http = []");
    let mut packages = clean_packages();
    packages.push(("mirror_http", &[]));
    assert_single(&packages, &map, |_| MANIFEST_OK.into(), 'f', &["mirror_http"]);
}

#[test]
fn red_proof_g_forbidden_path_roots_in_source() {
    let planted = [
        concat!("use ocx_", "cli::Thing;"),
        concat!("    let x = ::ocx_", "store::open();"),
        concat!("extern crate ocx_", "shell;"),
        concat!("fn f() { ocx_", "project::load() }"),
    ];
    for line in planted {
        let files = [(PathBuf::from("src/planted.rs"), format!("fn ok() {{}}\n{line}\n"))];
        let violations = scan_sources(&files);
        assert_eq!(violations.len(), 1, "{line}: {violations:#?}");
        assert_eq!(violations[0].clause, 'g');
        assert!(
            violations[0].message.starts_with("src/planted.rs:2: "),
            "{:?}",
            violations[0].message
        );
    }

    // The mirror's own `pipeline::ocx_cli` module, reached relatively, and a
    // commented-out mention stay legal.
    let legal = concat!(
        "use crate::pipeline::ocx_",
        "cli::Push;\n",
        "use super::ocx_",
        "cli;\n",
        "mod ocx_",
        "cli;\n",
        "// ocx_",
        "cli::removed\n",
        "use ocx_",
        "oci::Identifier;\n",
    );
    let violations = scan_sources(&[(PathBuf::from("src/legal.rs"), legal.to_owned())]);
    assert!(violations.is_empty(), "{violations:#?}");
}
