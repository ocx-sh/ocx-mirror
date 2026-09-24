// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Source scans that must see more than one crate: they read `src/` and every
//! `crates/*/src/` from the workspace root, which no member's unit tests can
//! (adr_bazel_crate_split.md § C4; plan C-011). Cross-crate fixture copies are
//! checked here for the same reason.

use std::path::Path;

/// No production leg constructs its own `reqwest::Client`.
///
/// The factory is only worth having if every caller goes through it, and
/// that is precisely what does not hold on its own: the commit introducing
/// this module converted the legs it was written for and left `package
/// sync`, `pipeline describe` and the Discord webhook on
/// `reqwest::Client::new()` — three paths that kept failing behind a
/// corporate proxy after the bug was declared fixed. Nothing about a bare
/// constructor looks wrong at the call site, so the guard is a scan rather
/// than a review note.
///
/// Test corpora are exempt: a test client talks to a loopback listener and
/// wants no roots at all.
///
/// The scan reads production code only — everything from the first
/// `#[cfg(test)]` on is test code, and `//` comment lines are dropped, so
/// a factory that is merely *mentioned* in a doc comment neither counts as
/// a factory nor as an offender. Each needle proves itself on a synthetic
/// positive control first: a needle emptied by a careless edit would
/// otherwise turn the whole scan green.
#[test]
fn every_production_client_is_built_through_the_factory() {
    /// The bare constructors a leg must not call: each is a client with
    /// no extra roots, `reqwest::get` included (it builds a fresh
    /// `Client::new()` per call).
    ///
    /// `octocrab::Octocrab::builder()` is the fourth because this scan was
    /// blind to it for a whole release: it selects octocrab's *config*
    /// path, whose `build()` raises a `hyper_util` legacy client that
    /// reads no proxy variables and trusts the platform store alone —
    /// the same defect as a bare `reqwest` constructor, on a stack no
    /// `reqwest` needle can see. The service path
    /// (`OctocrabBuilder::new_empty().with_service(..)`,
    /// `crates/ocx_mirror_source/src/github_release.rs`) is how a GitHub client is built
    /// here, and it takes its transport from this module.
    /// The octocrab needles are four because the config path has four
    /// spellings, not one: `Octocrab::builder()` (lib.rs:1088),
    /// `OctocrabBuilder::new()` (:466), `OctocrabBuilder::default()`
    /// (:536) and `octocrab::instance()` (:412) all land on
    /// `DefaultOctocrabBuilderConfig` and raise the same proxy-blind
    /// client. The first needle is written unqualified so it catches the
    /// imported spelling too — a scan that only matched
    /// `octocrab::Octocrab::builder()` would miss the form anyone writes
    /// after a `use octocrab::Octocrab;`.
    const BARE_CONSTRUCTORS: [&str; 7] = [
        "reqwest::Client::new()",
        "reqwest::Client::builder()",
        "reqwest::get(",
        "Octocrab::builder()",
        "OctocrabBuilder::new()",
        "OctocrabBuilder::default()",
        "octocrab::instance(",
    ];
    /// The OCI transports are built outside this module by design
    /// (`ocx_oci` owns their roots, timeouts and auth), so the extra-CA
    /// seam is one call each factory has to make itself:
    /// `ClientBuilder::extra_roots(..)` or a `ClientConfig`
    /// `extra_root_certificates` append, both fed from `extra_roots()`.
    /// A factory without it is a leg a corporate CA never reaches — the
    /// same defect as a bare `reqwest` constructor, one layer down.
    /// `= native::ClientConfig {` rather than the bare type name so a
    /// `-> native::ClientConfig {` signature is not counted as a second
    /// factory (the spelling `registry_copy`'s own tests use).
    const OCI_FACTORIES: [&str; 2] = ["ClientBuilder::new()", "= native::ClientConfig {"];
    /// The seam, as the root spells it through its `http` alias and as a
    /// member crate spells it through the real crate path.
    const OCI_SEAMS: [&str; 2] = ["crate::http::extra_roots()", "ocx_mirror_http::extra_roots()"];

    #[derive(Default)]
    struct Scan {
        offenders: Vec<String>,
        /// Production files read.
        files: usize,
        /// Files in which an OCI factory was seen.
        oci_factory_files: usize,
    }

    /// One file's production lines, comment lines dropped.
    ///
    /// The cut is the `#[cfg(test)]` that gates the test *module* — the
    /// first one whose next non-blank, non-attribute line is a `mod` —
    /// not the first `#[cfg(test)]` in the file: a single-item
    /// `#[cfg(test)] use …` above the module would otherwise end the
    /// scan there and leave everything below it unread.
    fn production_lines(source: &str) -> Vec<(usize, &str)> {
        let lines: Vec<&str> = source.lines().collect();
        let opens_test_module = |index: usize| {
            lines[index].trim() == "#[cfg(test)]"
                && lines[index + 1..]
                    .iter()
                    .map(|line| line.trim())
                    .find(|line| !line.is_empty() && !line.starts_with("#["))
                    .is_some_and(|line| line.starts_with("mod ") || line.contains(" mod "))
        };
        let end = (0..lines.len())
            .find(|&index| opens_test_module(index))
            .unwrap_or(lines.len());
        lines[..end]
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            .collect()
    }

    fn check(label: &str, source: &str, scan: &mut Scan) {
        scan.files += 1;
        let lines = production_lines(source);
        for (offset, line) in &lines {
            if BARE_CONSTRUCTORS.iter().any(|needle| line.contains(needle)) {
                scan.offenders.push(format!("{label}:{}", offset + 1));
            }
        }
        let factories: usize = lines
            .iter()
            .map(|(_, line)| {
                OCI_FACTORIES
                    .iter()
                    .map(|needle| line.matches(needle).count())
                    .sum::<usize>()
            })
            .sum();
        let seams: usize = lines
            .iter()
            .map(|(_, line)| OCI_SEAMS.iter().map(|seam| line.matches(seam).count()).sum::<usize>())
            .sum();
        if factories > 0 {
            scan.oci_factory_files += 1;
        }
        if seams < factories {
            scan.offenders.push(format!(
                "{label} ({factories} OCI client factories, {seams} extra_roots() seams)"
            ));
        }
    }

    /// Exempt by path: the factory module itself, and the dev-only test
    /// scaffolding crate.
    const EXEMPT: [&str; 2] = [
        "crates/ocx_mirror_http/src/lib.rs",
        "crates/ocx_mirror_test_support/src/lib.rs",
    ];

    fn scan(root: &Path, dir: &Path, scan_state: &mut Scan) {
        for entry in std::fs::read_dir(dir).expect("src/ must be readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "tests") {
                    continue;
                }
                scan(root, &path, scan_state);
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            let exempt = path
                .strip_prefix(root)
                .is_ok_and(|relative| EXEMPT.iter().any(|exempt| relative == Path::new(exempt)));
            if path.extension().is_none_or(|ext| ext != "rs") || exempt || name == "tests.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("a readable Rust source file");
            check(&path.display().to_string(), &source, scan_state);
        }
    }

    // The cut is the test *module*, not the first `#[cfg(test)]`: a
    // single-item `#[cfg(test)] use …` above it (push.rs has one) must
    // not end the scan early, and a `#[path]` attribute between the
    // `cfg` and its `mod` must not hide the module.
    let mut after_import = Scan::default();
    check(
        "control",
        "#[cfg(test)]\nuse x;\nlet c = reqwest::Client::new();\n#[cfg(test)]\nmod tests {\nreqwest::get(\n}\n",
        &mut after_import,
    );
    assert_eq!(
        after_import.offenders,
        vec!["control:3"],
        "a needle after a `#[cfg(test)] use` import is production code; one inside the test module is not"
    );
    let mut pathed = Scan::default();
    check(
        "control",
        "#[cfg(test)]\n#[path = \"x/tests.rs\"]\nmod tests;\nreqwest::get(\n",
        &mut pathed,
    );
    assert!(
        pathed.offenders.is_empty(),
        "a `#[path]` between `#[cfg(test)]` and `mod tests;` must not hide the test module"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let push_rs =
        std::fs::read_to_string(root.join("src/command/package/pipeline/push.rs")).expect("push.rs is readable");
    let first_cfg_test = push_rs
        .lines()
        .position(|line| line.trim() == "#[cfg(test)]")
        .expect("push.rs carries a `#[cfg(test)]` import above its test module");
    let last_scanned = production_lines(&push_rs).last().map(|(offset, _)| *offset);
    assert!(
        last_scanned.is_some_and(|last| last > first_cfg_test),
        "push.rs must be scanned past its `#[cfg(test)] use` import at line {}, scanned through {last_scanned:?}",
        first_cfg_test + 1
    );

    // Positive controls: every needle flags on a synthetic source, a
    // comment line flags nothing, and the OCI clause counts rather than
    // merely finds the seam.
    for needle in BARE_CONSTRUCTORS {
        let mut control = Scan::default();
        check("control", &format!("let c = {needle}url);\n"), &mut control);
        assert_eq!(
            control.offenders,
            vec!["control:1"],
            "needle {needle:?} must flag a bare constructor"
        );
        let mut commented = Scan::default();
        check("control", &format!("    // {needle}\n"), &mut commented);
        assert!(
            commented.offenders.is_empty(),
            "a comment line must not flag {needle:?}"
        );
    }
    // One control per seam spelling: each must count on its own.
    for needle in OCI_FACTORIES {
        for seam in OCI_SEAMS {
            let mut control = Scan::default();
            check(
                "control",
                &format!("let a {needle}\nlet b {needle}\n{seam}\n// {seam}\n"),
                &mut control,
            );
            assert_eq!(control.oci_factory_files, 1);
            assert_eq!(
                control.offenders,
                vec!["control (2 OCI client factories, 1 extra_roots() seams)"],
                "needle {needle:?} must count every factory against the seams (spelling {seam:?})"
            );
        }
    }

    // The walk covers the root package and every workspace member, so a leg
    // that moves into a crate stays scanned.
    let mut state = Scan::default();
    scan(root, &root.join("src"), &mut state);
    let mut members: Vec<_> = std::fs::read_dir(root.join("crates"))
        .expect("crates/ must be readable")
        .map(|entry| entry.expect("a readable directory entry").path().join("src"))
        .filter(|src| src.is_dir())
        .collect();
    members.sort();
    for src in &members {
        scan(root, src, &mut state);
    }
    // Floors, not `> 0`: the walk must not silently shrink as modules move
    // between crates. Both are the counts across `src/` and every
    // `crates/*/src/` after the report crate landed (crate split WP4,
    // 2026-09-23): 130 files, 3 of them with an OCI factory. The one lowering
    // since: `ocx_python`'s 9 source files left the mirror for
    // `external/ocx` (phase 2), 133 read → 124. Raise them when files are
    // added; never lower them to make a move between mirror crates pass.
    const MIN_FILES: usize = 124;
    const MIN_FACTORY_FILES: usize = 3;
    assert!(
        state.files >= MIN_FILES,
        "the scan must have read production files ({} read, floor {MIN_FILES})",
        state.files
    );
    assert!(
        state.oci_factory_files >= MIN_FACTORY_FILES,
        "the scan must have met the OCI factories it exists to check ({} seen, floor {MIN_FACTORY_FILES})",
        state.oci_factory_files
    );
    assert!(
        state.offenders.is_empty(),
        "these legs bypass `ocx_mirror_http` and so drop the trust roots a corporate CA needs: {:#?}",
        state.offenders
    );
}

#[test]
fn the_mirror_rewrite_denylist_names_a_call_that_still_exists() {
    // The guard on the guard in `ocx_mirror_pipeline`'s registry_sync tests
    // (`the_source_read_seam_is_ssrf_guarded_and_never_mirror_rewritten`).
    // Its needles are denials, so they pass vacuously once the spelling they
    // name is gone — which is exactly what happened to `from_env` at the ocx
    // v0.6.0 bump. Pin them to a call that is real *today*: `registry_client`
    // must still be the mirror-installing constructor, and it must still
    // install the map with `.mirrors(`. It reads the root's command module
    // from a pipeline test, so it lives here (plan C-011).
    let client_source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/command/package/mod.rs"))
            .expect("src/command/package/mod.rs is readable");
    assert!(
        client_source.contains("fn registry_client("),
        "registry_client has been renamed or removed — re-point the denylist in \
         the_source_read_seam_is_ssrf_guarded_and_never_mirror_rewritten"
    );
    assert!(
        client_source.contains(".mirrors("),
        "registry_client no longer installs the OCX_MIRRORS map with `.mirrors(` — \
         find where it moved and re-point the denylist"
    );
}

/// The root's `console_pkg` wheel is a copy of `ocx_python`'s, whose original
/// now lives in ocx (`external/ocx/crates/ocx_python/tests/`); the copy keeps
/// `ocx_mirror_pipeline`'s `python_prepare` tests and the acceptance suite off
/// another crate's `tests/` (adr_bazel_crate_split.md § C2). A copy drifts
/// silently: regenerating one wheel leaves the other tests pinning the old
/// entry points.
#[test]
fn the_root_wheel_fixture_is_a_byte_copy_of_ocx_pythons() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let wheel = "fixtures/wheels/console_pkg-1.0.0-py3-none-any.whl";
    let copy = std::fs::read(root.join("tests").join(wheel)).expect("the root wheel copy is readable");
    // Bazel hands the original over as a runfiles path (`OCX_PYTHON_WHEEL`,
    // an `@ocx_python_wheels` input): the submodule is not under its root.
    let original_path = std::env::var_os("OCX_PYTHON_WHEEL").map_or_else(
        || root.join("external/ocx/crates/ocx_python/tests").join(wheel),
        std::path::PathBuf::from,
    );
    let original = std::fs::read(&original_path)
        .unwrap_or_else(|error| panic!("ocx_python's wheel at {} is readable: {error}", original_path.display()));
    assert!(
        copy == original,
        "tests/{wheel} differs from external/ocx/crates/ocx_python/tests/{wheel}; re-copy it"
    );
}
