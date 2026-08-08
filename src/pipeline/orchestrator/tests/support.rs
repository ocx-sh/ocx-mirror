// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `orchestrator` test module.

use super::super::*;

pub fn platform(spec: &str) -> ocx_lib::oci::Platform {
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
    let task = MirrorTask {
        version: "1.0.0".into(),
        normalized_version: "1.0.0".into(),
        platform: platform("linux/amd64"),
        download_url: "https://example.invalid/asset.tar.xz".parse().expect("valid url"),
        asset_name: "asset.tar.xz".into(),
        target: crate::spec::Target {
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
        cascade: false,
        spec_dir: spec_dir.to_path_buf(),
        asset_type: crate::spec::AssetType::Archive { strip_components: None },
        variant: None,
    };

    tokio::fs::create_dir_all(task_dir).await.expect("create task dir");
    let asset = task_dir.join(&task.asset_name);
    if !asset.exists() {
        staged_asset(&asset).await;
    }

    // Reqwest builds its TLS stack lazily on first `Client::new` and panics
    // with "No provider set" if none is registered — even though the staged
    // asset means no request is ever made. Without this the test is green
    // only when some other test in the process happened to register one
    // first, which is a green indistinguishable from never having run.
    static CRYPTO: std::sync::Once = std::sync::Once::new();
    CRYPTO.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });

    let progress = ProgressManager::hidden();
    let spinner = progress.spinner("test".to_string());
    let (_bundle, metadata) = prepare_task(
        &task,
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
    package::bundle(&content, at, 1).await.expect("build fixture asset");
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
