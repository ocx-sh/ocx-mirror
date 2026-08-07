// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Helpers shared by more than one `ci` test module.

use super::super::*;
use std::path::Path;
use tempfile::tempdir;

/// The shared base of a multi-spec repository: everything the packages have
/// in common and nothing that identifies one of them.
pub const EXTENDS_BASE: &str = r#"
platforms:
  linux/amd64:
    runner: ubuntu-latest
build_timestamp: none
cascade: true
"#;

pub const SHFMT_SPEC: &str = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
"#;

/// Render `describe.yml` for an inline spec at the repository root.
pub fn describe_of(yaml: &str) -> String {
    render_describe(&spec_from_yaml(yaml), &root_slot())
}

/// One package spec of such a repository.
///
/// With `extends`, the platform matrix comes from the base; without, the
/// same keys are inlined — so the two render the same workflow apart from
/// the trigger, which is what makes the absent-base assertion meaningful.
pub fn extends_child(name: &str, extends: Option<&str>) -> String {
    let body = format!(
        r#"name: {name}
target:
  registry: ocx.sh
  repository: {name}
source:
  type: github_release
  owner: bazelbuild
  repo: buildtools
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "{name}-linux-amd64$"
asset_type:
  type: binary
  name: {name}
tests:
  - name: version
    command: {name} --version
"#
    );
    match extends {
        Some(base) => format!("extends: {base}\n{body}"),
        None => format!("{body}{EXTENDS_BASE}"),
    }
}

/// Run `generate ci` over `specs` with `repo_root` as the repository root.
pub fn generate(repo_root: &Path, specs: &[PathBuf], check: bool) -> Result<(), MirrorError> {
    let cmd = GenerateCi {
        spec: specs.to_vec(),
        repo_root: Some(repo_root.to_path_buf()),
        check,
        format: None,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    rt.block_on(async { cmd.execute(&printer).await })
}

/// Copy a fixture into `work_dir` as the repository's root `mirror.yml`.
///
/// That is the layout every published mirror repository has, and the one
/// the goldens are pinned to: a repo-root `mirror.yml` is the single spec
/// path the generated invocations may leave unsaid.
pub fn install_spec(fixture_name: &str, work_dir: &Path) -> PathBuf {
    install_spec_at(fixture_name, work_dir, "mirror.yml")
}

/// Copy a fixture into `work_dir` at `relative`, creating parents.
pub fn install_spec_at(fixture_name: &str, work_dir: &Path, relative: &str) -> PathBuf {
    let fixture_src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/")).join(fixture_name);
    let spec_dest = work_dir.join(relative);
    std::fs::create_dir_all(spec_dest.parent().unwrap()).unwrap();
    std::fs::copy(&fixture_src, &spec_dest).expect("failed to copy fixture into work_dir");
    spec_dest
}

/// Install a fixture as `work_dir/mirror.yml` and render it there.
///
/// Returns `Err(MirrorError)` if the renderer rejects the spec.
pub fn render_fixture(fixture_name: &str, work_dir: &Path) -> Result<(), MirrorError> {
    let spec = install_spec(fixture_name, work_dir);
    // The one `script:` in the fixture corpus. `script:` resolves from the
    // repository root and must exist, so the repository has to hold it.
    write_file(work_dir, "tests/smoke.star", "ocx_assert(True)\n");
    generate(work_dir, &[spec], false)
}

/// The slot a single-spec repository's root `mirror.yml` occupies.
pub fn root_slot() -> SpecSlot {
    slot_at(DEFAULT_SPEC_NAME)
}

/// The slot a spec at `relative` occupies, extending nothing.
pub fn slot_at(relative: &str) -> SpecSlot {
    SpecSlot {
        relative: PathBuf::from(relative),
        extends: Vec::new(),
    }
}

/// Build a `MirrorSpec` from inline YAML (no fixture file needed).
pub fn spec_from_yaml(yaml: &str) -> crate::spec::MirrorSpec {
    serde_yaml_ng::from_str(yaml).expect("inline spec must parse")
}

/// A repository holding a root spec and a nested one, rendered.
///
/// `mirror-ghcr-announce.yml` is the nested spec because it also emits an
/// `announce-from-registry` workflow, so the suffixing is exercised on all
/// three per-spec files rather than just two.
pub fn two_spec_repo(dir: &Path) -> Vec<PathBuf> {
    let root = install_spec("mirror-minimal.yml", dir);
    let nested = install_spec_at("mirror-ghcr-announce.yml", dir, "py3.13/mirror.yml");
    vec![root, nested]
}

/// Render a fixture and return the generated `mirror.yml` content.
pub fn workflow_for(fixture: &str) -> String {
    let dir = tempdir().unwrap();
    render_fixture(fixture, dir.path()).unwrap_or_else(|e| panic!("{fixture} must render: {e}"));
    std::fs::read_to_string(dir.path().join(".github/workflows/mirror.yml")).unwrap()
}

/// Render `mirror.yml` for an inline spec at the repository root.
pub fn workflow_of(yaml: &str) -> String {
    render_workflow(&spec_from_yaml(yaml), &root_slot())
}

/// Write `content` at `relative` under `dir`, creating parents.
pub fn write_file(dir: &Path, relative: &str, content: &str) -> PathBuf {
    let dest = dir.join(relative);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, content).unwrap();
    dest
}
