// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `orchestrator` test module.

use super::super::*;

pub fn platform(spec: &str) -> ocx_oci::Platform {
    spec.parse().expect("valid platform")
}

/// As [`prepare_scanned`], with the libc check switchable. Separate only so
/// the eleven `bin_scan` tests do not carry a fourth argument none of them
/// vary.
#[cfg(unix)]
pub async fn prepare_offline(
    spec_dir: &Path,
    task_dir: &Path,
    bin_scan: BinScanMode,
    libc_lint: bool,
) -> Result<Metadata> {
    let task = offline_task(spec_dir, bin_scan, libc_lint);
    run_prepare(&task, task_dir).await
}

/// The task [`prepare_offline`] runs, built separately so a test can vary a
/// field the eleven callers above never do — the declared digest, say.
#[cfg(unix)]
pub fn offline_task(spec_dir: &Path, bin_scan: BinScanMode, libc_lint: bool) -> MirrorTask {
    MirrorTask {
        version: "1.0.0".into(),
        normalized_version: "1.0.0".into(),
        platform: platform("linux/amd64"),
        download_url: "https://example.invalid/asset.tar.xz".parse().expect("valid url"),
        asset_name: "asset.tar.xz".into(),
        target: ocx_mirror_spec::Target {
            registry: "registry.test".into(),
            repository: "mirror/tool".into(),
        },
        metadata_config: Some(MetadataConfig {
            default: "metadata.json".into(),
            platforms: HashMap::new(),
        }),
        bin_scan,
        libc_lint,
        verify_config: None,
        asset_digest: None,
        require_digest: false,
        cascade: false,
        spec_dir: spec_dir.to_path_buf(),
        asset_type: ocx_mirror_spec::AssetType::Archive { strip_components: None },
        variant: None,
    }
}

/// Runs the real prepare phase against a staged asset, so no request is made.
#[cfg(unix)]
pub async fn run_prepare(task: &MirrorTask, task_dir: &Path) -> Result<Metadata> {
    tokio::fs::create_dir_all(task_dir).await.expect("create task dir");
    let asset = task_dir.join(&task.asset_name);
    if !asset.exists() {
        staged_asset(&asset).await;
    }
    prepare_as_is(task, task_dir).await
}

/// [`run_prepare`] without staging the asset — the work dir is run exactly as
/// the caller left it, which is the only way to reach a resume whose archive
/// is gone.
#[cfg(unix)]
pub async fn prepare_as_is(task: &MirrorTask, task_dir: &Path) -> Result<Metadata> {
    // Reqwest builds its TLS stack lazily on first `Client::new` and panics
    // with "No provider set" if none is registered — even though the staged
    // asset means no request is ever made. Without this the test is green
    // only when some other test in the process happened to register one
    // first, which is a green indistinguishable from never having run.
    ocx_mirror_test_support::install_crypto_provider();

    // `prepare_task` resolves `ocx` through the process-global
    // `OCX_BINARY_PIN`, which neighbouring modules' tests point at their own
    // stand-ins. Held here rather than by each test because this is the one
    // place a prepare spawns anything, and no caller holds it already.
    let _lock = ocx_mirror_test_support::OCX_ENV_LOCK.lock().await;
    let script = fake_ocx_create(task_dir).display().to_string();
    let _pin = ocx_mirror_test_support::EnvRestore::set(&[("OCX_BINARY_PIN", Some(script.as_str()))]);

    let progress = ProgressManager::hidden();
    let spinner = progress.spinner("test".to_string());
    let (_bundle, metadata) = prepare_task(
        task,
        task_dir,
        &reqwest::Client::new(),
        &spinner,
        &Semaphore::new(1),
        &Semaphore::new(1),
        1,
    )
    .await?;
    Ok(metadata)
}

/// A spec directory holding one metadata file that declares an
/// interface-visible `${installPath}/bin` PATH var — the shape a scan
/// looks at — plus whatever `binaries` clause the caller wants in it.
#[cfg(unix)]
pub fn spec_dir_declaring(binaries: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write_metadata(dir.path(), "bin", binaries, "");
    dir
}

/// Compresses `content` into the `.tar.xz` at `at` — an upstream asset,
/// built here so a test needs no network.
#[cfg(unix)]
pub async fn archive(content: &Path, at: &Path) {
    ocx_package::bundle::BundleBuilder::from_path(content)
        .create(at)
        .await
        .expect("build fixture asset");
}

/// A `.tar.xz` holding `bin/tool` with the exec bit set — the upstream
/// asset a mirror downloads, built here so the test needs no network.
#[cfg(unix)]
pub async fn staged_asset(at: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let content = at.parent().expect("asset has a parent").join("upstream-content");
    std::fs::create_dir_all(content.join("bin")).expect("create fixture tree");
    let tool = content.join("bin").join("tool");
    std::fs::write(&tool, b"#!/bin/sh\n").expect("write fixture tool");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod fixture tool");
    archive(&content, at).await;
    std::fs::remove_dir_all(&content).expect("drop fixture tree");
}

/// Rewrites the spec's metadata file: PATH pointed at `rel`, plus whatever
/// `binaries` clause and `extra` keys the caller wants. Separate from
/// [`spec_dir_declaring`] so a test can change the spec *between* runs.
#[cfg(unix)]
pub fn write_metadata(spec_dir: &Path, rel: &str, binaries: &str, extra: &str) {
    std::fs::write(
        spec_dir.join("metadata.json"),
        format!(
            r#"{{"type":"bundle","version":1{binaries},
                "env":[{{"key":"PATH","type":"path","value":"${{installPath}}/{rel}","required":false,"visibility":"interface"}}{extra}]}}"#
        ),
    )
    .expect("write metadata fixture");
}

/// A stand-in `ocx` whose `package create` does what the real one does to the
/// files the mirror reads back, and nothing it computes.
///
/// It logs its argv to `{task_dir}/fake-create-argv` (one line per call),
/// copies the tree it was handed to `{output}.tree` with modes intact — the
/// bytes the real create would have bundled — writes an empty bundle, and
/// writes the compiled sidecar beside it under ocx's naming. The sidecar is
/// `{task_dir}/fake-compiled.json` when a test staged one (the pins, the
/// scanned `binaries` the real create would have produced), else the authoring
/// input verbatim. A `{task_dir}/fake-create-stderr` makes it refuse, printing
/// that file and exiting 65 before writing anything.
#[cfg(unix)]
pub fn fake_ocx_create(task_dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(task_dir).expect("create task dir");
    let dir = task_dir.display();
    let script = task_dir.join("fake-ocx");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{dir}/fake-create-argv'
content=""; out=""; meta=""
while [ $# -gt 0 ]; do
  case "$1" in
    package|create) shift ;;
    --output) out="$2"; shift 2 ;;
    --metadata) meta="$2"; shift 2 ;;
    --platform|--threads) shift 2 ;;
    --*) shift ;;
    *) content="$1"; shift ;;
  esac
done
if [ -f '{dir}/fake-create-stderr' ]; then cat '{dir}/fake-create-stderr' >&2; exit 65; fi
rm -rf "$out.tree" && cp -Rp "$content" "$out.tree" || exit 1
: > "$out"
sidecar="${{out%.tar.xz}}-metadata.json"
if [ -f '{dir}/fake-compiled.json' ]; then cp '{dir}/fake-compiled.json' "$sidecar"; else cp "$meta" "$sidecar"; fi
"#
        ),
    )
    .expect("write the fake ocx");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("make the fake ocx executable");
    script
}

/// Every argv the fake create was called with, one entry per invocation.
#[cfg(unix)]
pub fn create_invocations(task_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(task_dir.join("fake-create-argv"))
        .map(|log| log.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Stages the compiled sidecar the next fake create writes: the spec's own
/// metadata file with `binaries` set, standing in for a scan's fill.
#[cfg(unix)]
pub fn stage_compiled_binaries(task_dir: &Path, spec_dir: &Path, binaries: &[&str]) {
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(spec_dir.join("metadata.json")).expect("spec metadata"))
            .expect("spec metadata parses");
    doc["binaries"] = serde_json::json!(binaries);
    std::fs::create_dir_all(task_dir).expect("create task dir");
    std::fs::write(task_dir.join("fake-compiled.json"), doc.to_string()).expect("stage the compiled sidecar");
}

/// The file mode of `relative` in the tree the fake create was handed.
#[cfg(unix)]
pub fn mode_create_saw(task_dir: &Path, relative: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(task_dir.join("bundle.tar.xz.tree").join(relative))
        .unwrap_or_else(|e| panic!("{relative} missing from the tree create received: {e}"))
        .permissions()
        .mode()
        & 0o777
}
