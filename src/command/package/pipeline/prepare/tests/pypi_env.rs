// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::command::package::pipeline::plan::{PlanVersionEntry, PlanVersionKind};
use std::path::Path;
use tempfile::tempdir;

// ── plan_python_mirror_v2 W2.A3: pypi env-prepare dispatch ───────────────

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

const PYPI_DERIVED_LOCK_BODY: &str = r#"lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-py3-none-any.whl"
url = "https://example.com/pycowsay-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;

/// A `pypi` plan document carrying `pylock` for its single entry.
fn pypi_plan_with_lock(lock_relative: &str) -> PlanReport {
    PlanReport {
        schema_version: 3,
        has_new: true,
        has_drift: false,
        versions: vec![PlanVersionEntry {
            version: "1.0.0".to_string(),
            platforms: vec!["linux/amd64".to_string()],
            kind: PlanVersionKind::New,
            source_version: "1.0.0".to_string(),
            variant: None,
            assets: vec![],
            pylock: Some(lock_relative.to_string()),
        }],
        target: "ocx.sh/pycowsay".to_string(),
        ocx_mirror_rev: None,
    }
}

/// Writes `body` as the derived lock under `{dir}/locks/` plus the
/// `plan.json` that references it, returning the plan path.
fn write_pypi_plan(dir: &Path, body: &str) -> PathBuf {
    let locks_dir = dir.join("locks");
    std::fs::create_dir_all(&locks_dir).unwrap();
    std::fs::write(locks_dir.join("pylock.pycowsay-1-0-0.toml"), body).unwrap();

    let plan = pypi_plan_with_lock("locks/pylock.pycowsay-1-0-0.toml");
    let plan_path = dir.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();
    plan_path
}

#[tokio::test]
async fn build_pypi_env_tasks_consumes_plan_provided_lock_without_deriving() {
    // No OCX_BINARY_PIN/OCX_MIRROR_UV stub is installed for this test: if
    // the plan-provided-lock path fell through to re-derivation, it would
    // try to spawn a real `ocx`/`uv` binary and fail — proving this path
    // never touches them.
    let plan_dir = tempdir().unwrap();
    let plan_path = write_pypi_plan(plan_dir.path(), PYPI_DERIVED_LOCK_BODY);

    let spec = pypi_fixture_spec();
    let candidates = fake_interpreter_candidates();

    let tasks = build_pypi_env_tasks(
        &spec,
        Path::new("."),
        "1.0.0",
        &candidates,
        None,
        Some(&plan_path),
        Path::new("."),
    )
    .await
    .expect("consuming a plan-provided lock never spawns uv/ocx");

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].source_version, "1.0.0");
    assert_eq!(tasks[0].platform.to_string(), "linux/amd64");
}

#[tokio::test]
async fn build_pypi_env_tasks_errors_on_unparseable_plan_provided_lock() {
    // A sdist-only package (no [[packages.wheels]]) is valid TOML but
    // fails ocx_python::parse_pylock's fail-closed re-parse — must
    // surface as PylockError (exit 65), not a panic or silent skip.
    let plan_dir = tempdir().unwrap();
    let bad_body = "lock-version = \"1.0\"\n\n[[packages]]\nname = \"pycowsay\"\nversion = \"1.0.0\"\n";
    let plan_path = write_pypi_plan(plan_dir.path(), bad_body);

    let spec = pypi_fixture_spec();
    let candidates = fake_interpreter_candidates();

    let err = build_pypi_env_tasks(
        &spec,
        Path::new("."),
        "1.0.0",
        &candidates,
        None,
        Some(&plan_path),
        Path::new("."),
    )
    .await
    .expect_err("an unparseable plan-provided lock must fail, not silently succeed");

    assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
    assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
}
