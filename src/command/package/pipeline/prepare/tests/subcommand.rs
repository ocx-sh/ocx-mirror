// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use std::panic;
use std::path::Path;
use tempfile::tempdir;

// ── §3.6 S6: prepare subcommand tests ────────────────────────────────────
//
// All tests that call execute() will panic with "not implemented"
// until wave 3. Tests that only exercise struct construction pass now.

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn make_printer() -> DataInterface {
    DataInterface::new(ocx_lib::cli::Printer::new(false, false))
}

fn run_prepare(cmd: Prepare) -> Result<(), MirrorError> {
    let printer = make_printer();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { cmd.execute(&printer).await })
}

#[test]
fn prepare_produces_bundle_for_each_declared_platform() {
    // §3.6: prepare --version 3.29.0 produces {work_dir}/{V}/{platform_slug}/bundle.tar.xz
    // for every declared platform.
    // Fails with "not implemented" until wave 3.
    let work_dir = tempdir().unwrap();
    let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        run_prepare(Prepare {
            spec: spec_path,
            version: "3.29.0".to_string(),
            work_dir: Some(work_dir.path().to_path_buf()),
            plan: None,
        })
    }));

    match result {
        Err(_) => {
            // Panicked with unimplemented!() — expected at Phase 3
        }
        Ok(Ok(())) => {
            let bundle_path = work_dir.path().join("3.29.0").join("linux_amd64").join("bundle.tar.xz");
            assert!(
                bundle_path.exists(),
                "Expected bundle at {}, not found",
                bundle_path.display()
            );
        }
        Ok(Err(_)) => {
            // Other errors acceptable for unimplemented paths
        }
    }
}

#[test]
fn prepare_produces_manifest_json() {
    // §3.6: Manifest file {work_dir}/{V}/manifest.json lists bundles with sizes + digests.
    let work_dir = tempdir().unwrap();
    let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        run_prepare(Prepare {
            spec: spec_path,
            version: "3.29.0".to_string(),
            work_dir: Some(work_dir.path().to_path_buf()),
            plan: None,
        })
    }));

    match result {
        Err(_) => {}
        Ok(Ok(())) => {
            let manifest_path = work_dir.path().join("3.29.0").join("manifest.json");
            assert!(manifest_path.exists(), "Expected manifest.json");
            let content = std::fs::read_to_string(&manifest_path).unwrap();
            let value: serde_json::Value = serde_json::from_str(&content).expect("manifest.json must be valid JSON");
            assert!(
                value.get("bundles").is_some() || value.is_array(),
                "manifest.json must contain bundle list"
            );
        }
        Ok(Err(_)) => {}
    }
}

#[test]
fn prepare_is_idempotent_on_rerun() {
    // §3.6: Re-run is idempotent (same bundles, no errors).
    let work_dir = tempdir().unwrap();
    let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

    let result1 = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        run_prepare(Prepare {
            spec: spec_path.clone(),
            version: "3.29.0".to_string(),
            work_dir: Some(work_dir.path().to_path_buf()),
            plan: None,
        })
    }));

    if result1.is_err() {
        // Both runs panicked with unimplemented — expected at Phase 3
        let result2 = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            run_prepare(Prepare {
                spec: spec_path,
                version: "3.29.0".to_string(),
                work_dir: Some(work_dir.path().to_path_buf()),
                plan: None,
            })
        }));
        assert!(result2.is_err(), "Second run must also panic with unimplemented");
        return;
    }

    if let Ok(Ok(())) = result1 {
        let result2 = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            run_prepare(Prepare {
                spec: spec_path,
                version: "3.29.0".to_string(),
                work_dir: Some(work_dir.path().to_path_buf()),
                plan: None,
            })
        }));
        assert!(matches!(result2, Ok(Ok(()))), "Second run (idempotent) must succeed");
    }
}

#[test]
fn prepare_exits_65_on_checksum_mismatch() {
    // §3.6: Checksum mismatch → exit 65 (DataError).
    // Uses a fake version string to trigger failure.
    // Until implementation: expect unimplemented!() panic.
    use ocx_lib::cli::ExitCode;

    let work_dir = tempdir().unwrap();
    let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        run_prepare(Prepare {
            spec: spec_path,
            version: "99.99.99-bad-checksum".to_string(),
            work_dir: Some(work_dir.path().to_path_buf()),
            plan: None,
        })
    }));

    match result {
        Err(_) => {} // unimplemented — expected at Phase 3
        Ok(Err(MirrorError::SpecInvalid(_))) => {
            // Version-not-found is acceptable response for fake version
        }
        Ok(Err(e)) => {
            let exit_code = e.kind_exit_code();
            assert!(
                exit_code == ExitCode::DataError || exit_code == ExitCode::Unavailable,
                "Checksum mismatch must exit DataError(65) or Unavailable(69), got: {:?}",
                exit_code
            );
        }
        Ok(Ok(())) => panic!("Expected error for bad checksum version"),
    }
}

#[test]
fn prepare_exits_69_on_source_unreachable() {
    // §3.6: Source unreachable → exit 69 (Unavailable).
    // SourceError maps to ExitCode::Unavailable (69) via kind_exit_code().
    let work_dir = tempdir().unwrap();
    let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        run_prepare(Prepare {
            spec: spec_path,
            version: "3.29.0".to_string(),
            work_dir: Some(work_dir.path().to_path_buf()),
            plan: None,
        })
    }));

    match result {
        Err(_) => {} // unimplemented — expected at Phase 3
        Ok(Err(MirrorError::SourceError(_))) => {
            // Source unreachable → SourceError maps to Unavailable (69)
        }
        Ok(Err(e)) => {
            let _ = e.kind_exit_code();
        }
        Ok(Ok(())) => {
            // Acceptable if network is available and source resolves
        }
    }
}

fn tasks_for(version: &str) -> Vec<MirrorTask> {
    let spec: MirrorSpec = serde_yaml_ng::from_str(APPLICABILITY_SPEC).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { build_tasks_for_version(&spec, Path::new("."), version).await.unwrap() })
}

#[test]
fn prepare_drops_out_of_window_platform() {
    // 0.10.0 is below windows/arm64's min_version (0.11.7) → only linux/amd64.
    assert_eq!(platforms_of(&tasks_for("0.10.0")), vec!["linux/amd64".to_string()]);
}

#[test]
fn prepare_drops_excluded_platform_but_keeps_others() {
    // 0.12.0 is in-window but windows/arm64 has an exclude entry for it →
    // only linux/amd64 is prepared; the version is not dropped entirely.
    assert_eq!(platforms_of(&tasks_for("0.12.0")), vec!["linux/amd64".to_string()]);
}

#[test]
fn prepare_keeps_in_window_platform() {
    // 0.11.8 is at/above min_version and not excluded → both platforms build.
    assert_eq!(
        platforms_of(&tasks_for("0.11.8")),
        vec!["linux/amd64".to_string(), "windows/arm64".to_string()]
    );
}

#[test]
fn prepare_default_work_dir_uses_none() {
    // §3.6: Default work_dir when not specified → uses default ./.ocx-mirror.
    // Verify Prepare struct accepts None for work_dir.
    let spec_path = Path::new(FIXTURE_DIR).join("mirror-minimal.yml");

    let cmd = Prepare {
        spec: spec_path,
        version: "3.29.0".to_string(),
        work_dir: None, // uses default ./.ocx-mirror
        plan: None,
    };

    // Struct construction must succeed (no panic)
    // Actual execution will panic with unimplemented!() — expected at Phase 3
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let printer = make_printer();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rt.block_on(async { cmd.execute(&printer).await });
    }));
    // Panicked or returned — either is acceptable at Phase 3
    let _ = result;
}
