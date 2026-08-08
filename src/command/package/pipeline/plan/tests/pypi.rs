// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── Decision A: pypi source — plan-phase candidate selection + lock derivation ──

/// Serializes tests that mutate `OCX_BINARY_PIN` / `OCX_MIRROR_UV`.
///
/// The crate-wide lock, shared with `pipeline push`'s tests: both modules
/// pin `OCX_BINARY_PIN` at their own stand-in binary, and a module-local
/// lock leaves a push test resolving *this* module's `uv` stub. Held
/// across a `block_on` rather than an `await`, which is why these tests are
/// `#[test]` + an explicit runtime instead of `#[tokio::test]`.
fn pypi_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    crate::test_support::ocx_env_lock()
}

fn write_executable_script(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod script");
}

/// Writes a stub `uv` that consumes stdin and writes `body` to the `-o`
/// argument — same shape as `pipeline::lock_derive`'s own test stub, plus
/// real uv's pylock output-filename rule: a `-o` basename that does not
/// start with `pylock.` and end with `.toml` is rejected with uv's own
/// message (regression guard for the live W4 pilot failure — the earlier,
/// laxer stub let a non-conforming name through that real uv rejects).
fn write_uv_stub(path: &std::path::Path, body: &str, exit_code: u32) {
    let script = format!(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         prev=\"\"\n\
         outfile=\"\"\n\
         for arg in \"$@\"; do\n\
         \x20 if [ \"$prev\" = \"-o\" ]; then outfile=\"$arg\"; fi\n\
         \x20 prev=\"$arg\"\n\
         done\n\
         if [ -n \"$outfile\" ]; then\n\
         \x20 base=${{outfile##*/}}\n\
         \x20 name=${{base#pylock.}}\n\
         \x20 name=${{name%.toml}}\n\
         \x20 case \"$base\" in\n\
         \x20   pylock.toml) ;;\n\
         \x20   pylock.*.toml)\n\
         \x20     case \"$name\" in\n\
         \x20       \"\"|*.*) echo \"error: Expected the output filename to be \\`pylock.toml\\` or \\`pylock.<name>.toml\\`, where \\`<name>\\` is non-empty and contains no dots; found \\`$base\\`\" >&2; exit 2 ;;\n\
         \x20     esac\n\
         \x20     ;;\n\
         \x20   *) echo 'error: Expected the output filename to start with `pylock.` and end with `.toml` (e.g., `pylock.toml`, `pylock.dev.toml`)' >&2; exit 2 ;;\n\
         \x20 esac\n\
         \x20 cat > \"$outfile\" <<'LOCKEOF'\n{body}LOCKEOF\n\
         fi\n\
         exit {exit_code}\n"
    );
    write_executable_script(path, &script);
}

fn pypi_fixture_spec() -> MirrorSpec {
    let yaml = r#"
name: pycowsay
target:
  registry: ocx.sh
  repository: pycowsay
source:
  type: pypi
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;
    serde_yaml_ng::from_str(yaml).unwrap()
}

fn version_info(version: &str, is_prerelease: bool) -> source::VersionInfo {
    source::VersionInfo {
        version: version.to_string(),
        assets: std::collections::HashMap::new(),
        is_prerelease,
    }
}

#[test]
fn locks_dir_default_is_relative_locks() {
    assert_eq!(DEFAULT_LOCKS_DIR, "locks");
}

#[test]
fn select_pypi_candidates_orders_oldest_first_and_applies_new_per_run() {
    let mut spec = pypi_fixture_spec();
    spec.versions = Some(crate::spec::VersionsConfig {
        new_per_run: Some(2),
        ..Default::default()
    });
    let upstream = vec![
        version_info("3.0.0", false),
        version_info("1.0.0", false),
        version_info("2.0.0", false),
    ];
    let version_map = VersionPlatformMap::default();

    let candidates = select_pypi_candidates(&spec, &upstream, &version_map);
    let versions: Vec<&str> = candidates.iter().map(|c| c.version.as_str()).collect();
    // Default backfill (newest_first) with cap=2: oldest-first order among the
    // two highest surviving versions.
    assert_eq!(versions, vec!["2.0.0", "3.0.0"]);
}

#[test]
fn select_pypi_candidates_bounds_four_segment_pep440_versions() {
    // Live regression (pipx, `min: "1.16.0"`): PyPI publishes 4-segment PEP 440
    // releases, `ocx_lib::Version` rejects them, and this filter kept whatever it
    // could not parse — so `0.15.5.1`/`0.16.2.0` planned as new work under a
    // 1.16 floor.
    let mut spec = pypi_fixture_spec();
    spec.versions = Some(crate::spec::VersionsConfig {
        min: Some("1.16.0".to_string()),
        ..Default::default()
    });
    let upstream = vec![
        version_info("0.15.5.1", false),
        version_info("0.16.2.0", false),
        version_info("1.16.6", false),
    ];

    let candidates = select_pypi_candidates(&spec, &upstream, &VersionPlatformMap::default());
    let versions: Vec<&str> = candidates.iter().map(|c| c.version.as_str()).collect();
    assert_eq!(versions, vec!["1.16.6"], "sub-min PEP 440 releases must be dropped");
}

#[test]
fn select_pypi_candidates_skips_fully_published_version() {
    let spec = pypi_fixture_spec();
    let upstream = vec![version_info("1.0.0", false), version_info("2.0.0", false)];
    let mut version_map = VersionPlatformMap::default();
    version_map.add(Version::parse("1.0.0").unwrap(), "linux/amd64".parse().unwrap());

    let candidates = select_pypi_candidates(&spec, &upstream, &version_map);
    let versions: Vec<&str> = candidates.iter().map(|c| c.version.as_str()).collect();
    assert_eq!(versions, vec!["2.0.0"], "already-published version must be dropped");
}

#[test]
fn select_pypi_candidates_never_panics_on_unparseable_version() {
    // Regression: a PEP 440 version beyond ocx_lib::Version's 3-component
    // parser (e.g. a calendar version) must never panic filter::filter_versions
    // would (its dedup step `.expect()`s a parseable tag) — this is exactly why
    // select_pypi_candidates doesn't reuse it.
    let spec = pypi_fixture_spec();
    let upstream = vec![version_info("2024.1.1.1", false)];
    let version_map = VersionPlatformMap::default();

    let candidates = select_pypi_candidates(&spec, &upstream, &version_map);
    assert_eq!(candidates.len(), 1, "unparseable version kept as outstanding work");
}

const PYPI_STUB_LOCK_BODY: &str = r#"lock-version = "1.0"
requires-python = ">=3.9.1"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-py3-none-any.whl"
url = "https://example.com/pycowsay-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;

/// Writes the stub `ocx` + `uv` scripts `build_pypi_plan_entries` needs, and
/// sets `OCX_BINARY_PIN`/`OCX_MIRROR_UV` (caller holds `pypi_env_lock`).
/// Returns the `TempDir` guards so callers keep them alive for the test's
/// duration.
fn install_pypi_stubs(uv_lock_body: &str, uv_exit_code: u32) -> (tempfile::TempDir, tempfile::TempDir) {
    let interpreter_root = tempfile::tempdir().unwrap();
    let bin = interpreter_root.path().join("content/python/install/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("python3"), "").unwrap();

    let scripts_dir = tempfile::tempdir().unwrap();
    let ocx_stub = scripts_dir.path().join("ocx");
    write_executable_script(
        &ocx_stub,
        &format!(
            "#!/bin/sh\necho '{{\"ocx.sh/python/cpython:3.13.1\": \"{}\"}}'\n",
            interpreter_root.path().display()
        ),
    );
    let uv_stub = scripts_dir.path().join("uv");
    write_uv_stub(&uv_stub, uv_lock_body, uv_exit_code);

    // SAFETY: test-only env vars, serialized by `pypi_env_lock()`.
    unsafe {
        std::env::set_var("OCX_BINARY_PIN", &ocx_stub);
        std::env::set_var("OCX_MIRROR_UV", &uv_stub);
    }
    (interpreter_root, scripts_dir)
}

fn remove_pypi_stubs() {
    // SAFETY: test-only env vars, serialized by `pypi_env_lock()`.
    unsafe {
        std::env::remove_var("OCX_BINARY_PIN");
        std::env::remove_var("OCX_MIRROR_UV");
    }
}

#[test]
fn build_pypi_plan_entries_writes_lock_and_references_it_in_the_entry() {
    let _guard = pypi_env_lock();
    let _stubs = install_pypi_stubs(PYPI_STUB_LOCK_BODY, 0);

    let spec = pypi_fixture_spec();
    let upstream = vec![version_info("1.0.0", false)];
    let version_map = VersionPlatformMap::default();
    let locks_root = tempfile::tempdir().unwrap();
    let locks_dir = locks_root.path().join("locks");

    let result = block_on(build_pypi_plan_entries(
        &spec,
        &upstream,
        &[],
        &version_map,
        &locks_dir,
        &None,
    ));
    remove_pypi_stubs();

    let entries = result.expect("pypi plan entries derive successfully");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].version, "1.0.0");
    let pylock_path = entries[0]
        .pylock
        .clone()
        .expect("a pypi-derived entry must carry a pylock path");
    assert!(
        std::path::Path::new(&pylock_path).exists(),
        "the derived lock must exist on disk at the referenced path"
    );
    // Dots are dashed out of the `<name>` segment — uv rejects a dotted one.
    assert!(pylock_path.contains("pylock.pycowsay-1-0-0.toml"), "got: {pylock_path}");

    // Round-trip through JSON exactly as `plan.json` would carry it.
    let report = PlanReport {
        schema_version: 3,
        has_new: true,
        has_drift: false,
        versions: entries,
        target: "ocx.sh/pycowsay".to_string(),
        ocx_mirror_rev: None,
    };
    let json = serde_json::to_string(&report).unwrap();
    let parsed: PlanReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.versions[0].pylock.as_deref(), Some(pylock_path.as_str()));
}

#[test]
fn build_pypi_plan_entries_reparse_failure_maps_to_data_error_exit_65() {
    // A sdist-only package (no [[packages.wheels]]) parses as valid TOML
    // but is rejected by ocx_python::parse_pylock's fail-closed re-parse —
    // must surface as PylockError (exit 65), not a generic ExecutionFailed (1).
    let _guard = pypi_env_lock();
    let bad_body = "lock-version = \"1.0\"\n\n[[packages]]\nname = \"pycowsay\"\nversion = \"1.0.0\"\n";
    let _stubs = install_pypi_stubs(bad_body, 0);

    let spec = pypi_fixture_spec();
    let upstream = vec![version_info("1.0.0", false)];
    let version_map = VersionPlatformMap::default();
    let locks_root = tempfile::tempdir().unwrap();
    let locks_dir = locks_root.path().join("locks");

    let result = block_on(build_pypi_plan_entries(
        &spec,
        &upstream,
        &[],
        &version_map,
        &locks_dir,
        &None,
    ));
    remove_pypi_stubs();

    let err = result.expect_err("an unparseable derived lock must fail, not silently succeed");
    assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
    assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
}

#[test]
fn build_pypi_plan_entries_universal_mode_never_invokes_ocx() {
    // Regression (live W4 pilot, static-python bug): `uv pip compile
    // --python <path>` fails against a fully-static interpreter ("Could
    // not detect a glibc or a musl libc"). Universal locks (the default)
    // must resolve via `--python-version X.Y` instead — which means the
    // plan phase must NOT materialize the interpreter at all. The ocx
    // stub here hard-fails if invoked, so any reintroduced
    // `materialize_interpreter` call in the universal path turns this red.
    let _guard = pypi_env_lock();
    let scripts_dir = tempfile::tempdir().unwrap();
    let ocx_stub = scripts_dir.path().join("ocx");
    write_executable_script(
        &ocx_stub,
        "#!/bin/sh\necho 'ocx must not be invoked for universal lock derivation' >&2\nexit 1\n",
    );
    let uv_stub = scripts_dir.path().join("uv");
    write_uv_stub(&uv_stub, PYPI_STUB_LOCK_BODY, 0);
    // SAFETY: test-only env vars, serialized by `pypi_env_lock()`.
    unsafe {
        std::env::set_var("OCX_BINARY_PIN", &ocx_stub);
        std::env::set_var("OCX_MIRROR_UV", &uv_stub);
    }

    let spec = pypi_fixture_spec(); // no lock: block -> universal defaults to true
    let upstream = vec![version_info("1.0.0", false)];
    let version_map = VersionPlatformMap::default();
    let locks_root = tempfile::tempdir().unwrap();
    let locks_dir = locks_root.path().join("locks");

    let result = block_on(build_pypi_plan_entries(
        &spec,
        &upstream,
        &[],
        &version_map,
        &locks_dir,
        &None,
    ));
    remove_pypi_stubs();

    let entries = result.expect("universal derivation must succeed without touching ocx");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].pylock.is_some());
}

#[test]
fn derived_lock_filename_name_segment_carries_no_dots() {
    // Regression (live W4 pypi pilot, mirror-pypi run 30874908847): uv
    // enforces PEP 751 on `-o`, and `<name>` in `pylock.<name>.toml` must
    // be non-empty and dot-free. A dotted version went straight through
    // into `<name>` and uv exited 2:
    //   error: Expected the output filename to be `pylock.toml` or
    //   `pylock.<name>.toml`, where `<name>` is non-empty and contains no
    //   dots; found `pylock.pycowsay-0.0.0.1.toml`
    for (package, version) in [
        ("pycowsay", "0.0.0.1"),
        ("black", "26.5.1"),
        // A dotted distribution name (`zope.interface`) must be sanitized
        // on the package side too, not just the version side.
        ("zope.interface", "7.0"),
    ] {
        let filename = derived_lock_filename(package, version);
        let name = filename
            .strip_prefix("pylock.")
            .and_then(|rest| rest.strip_suffix(".toml"))
            .unwrap_or_else(|| panic!("filename must be `pylock.<name>.toml`, got: {filename}"));
        assert!(!name.is_empty(), "`<name>` must be non-empty, got: {filename}");
        assert!(!name.contains('.'), "uv rejects a dotted `<name>`; got: {filename}");
    }
}

#[test]
fn build_pypi_plan_entries_derived_lock_filename_follows_uv_naming_rule() {
    // Regression (live W4 pilot): real `uv pip compile` REJECTS `-o`
    // filenames outside `pylock.toml` / `pylock.<name>.toml` with a
    // non-empty, dot-free `<name>`. The earlier
    // `{package}-{version}.pylock.toml` and `pylock.{package}-{version}.toml`
    // shapes both passed a laxer stub but failed live CI — the stub now
    // enforces uv's full rule, so this exercises it end to end on the
    // pilot's own dotted version.
    let _guard = pypi_env_lock();
    let _stubs = install_pypi_stubs(PYPI_STUB_LOCK_BODY, 0);

    let spec = pypi_fixture_spec();
    let upstream = vec![version_info("0.0.0.1", false)];
    let version_map = VersionPlatformMap::default();
    let locks_root = tempfile::tempdir().unwrap();
    let locks_dir = locks_root.path().join("locks");

    let result = block_on(build_pypi_plan_entries(
        &spec,
        &upstream,
        &[],
        &version_map,
        &locks_dir,
        &None,
    ));
    remove_pypi_stubs();

    let entries = result.expect("derivation must succeed with a uv-conforming output filename");
    let pylock_path = entries[0].pylock.as_deref().expect("entry carries a pylock path");
    let filename = std::path::Path::new(pylock_path)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("pylock path has a UTF-8 filename");
    let name = filename
        .strip_prefix("pylock.")
        .and_then(|rest| rest.strip_suffix(".toml"))
        .unwrap_or_else(|| panic!("derived lock filename must match uv's `pylock.<name>.toml` rule, got: {filename}"));
    assert!(
        !name.is_empty() && !name.contains('.'),
        "uv requires a non-empty, dot-free `<name>`; got: {filename}"
    );
}

#[test]
fn build_pypi_plan_entries_uv_resolution_failure_maps_to_data_error_exit_65() {
    // W3 acceptance contract: uv-fail→65. A nonzero uv exit (unsolvable
    // requirements, bad package metadata) means this version cannot
    // produce a lock — a data error (PylockError, 65), NOT a generic
    // ExecutionFailed (1), which stays reserved for uv-missing/spawn/
    // timeout failures. The surfaced message must carry uv's stderr.
    let _guard = pypi_env_lock();
    let (_interpreter_root, scripts_dir) = install_pypi_stubs("", 0);
    write_executable_script(
        &scripts_dir.path().join("uv"),
        "#!/bin/sh\ncat > /dev/null\necho 'no solution found for pycowsay==1.0.0' >&2\nexit 1\n",
    );

    let spec = pypi_fixture_spec();
    let upstream = vec![version_info("1.0.0", false)];
    let version_map = VersionPlatformMap::default();
    let locks_root = tempfile::tempdir().unwrap();
    let locks_dir = locks_root.path().join("locks");

    let result = block_on(build_pypi_plan_entries(
        &spec,
        &upstream,
        &[],
        &version_map,
        &locks_dir,
        &None,
    ));
    remove_pypi_stubs();

    let err = result.expect_err("a nonzero uv exit must fail the plan");
    assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
    assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
    assert!(
        err.to_string().contains("no solution found"),
        "the error must carry uv's stderr, got: {err}"
    );
}

#[tokio::test]
async fn build_pypi_plan_entries_skips_derivation_when_no_candidates() {
    // No uv/ocx stubs installed: if select_pypi_candidates didn't correctly
    // drop the fully-published version, this would fail trying to spawn a
    // real `ocx`/`uv` binary.
    let spec = pypi_fixture_spec();
    let upstream = vec![version_info("1.0.0", false)];
    let mut version_map = VersionPlatformMap::default();
    version_map.add(Version::parse("1.0.0").unwrap(), "linux/amd64".parse().unwrap());
    let locks_root = tempfile::tempdir().unwrap();
    let locks_dir = locks_root.path().join("locks");

    let entries = build_pypi_plan_entries(&spec, &upstream, &[], &version_map, &locks_dir, &None)
        .await
        .expect("no candidates means no subprocess spawns, so this never touches uv/ocx");
    assert!(entries.is_empty());
    assert!(
        !locks_dir.exists(),
        "locks dir must not even be created when there's nothing to derive"
    );
}
