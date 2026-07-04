// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Env-package push phase for `source.type: pylock` mirrors (W2.4 Stage 1).
//!
//! A parallel path to the archive/binary push loop in
//! `command::package::pipeline::push::Push::execute`: reads the
//! `env-manifest.json` per version written by
//! [`prepare_env_version`](super::python_prepare::prepare_env_version), then
//! pushes each green `(version, platform)` env package via
//! `ocx package push` with the ordered wheel layers as positional layer args
//! and the composed `metadata.json` via `-m`.
//!
//! Stage 2 (deferred): wheel-repo upload-if-missing (content-addressed wheel
//! layer reuse across packages) is not implemented here — every push
//! re-uploads its full layer set, which is correct but not maximally
//! efficient. See the `// W2.4 Stage 2` marker in `push.rs`.

use std::path::{Path, PathBuf};

use super::python_prepare::EnvManifest;

/// Parsed JSON output from `ocx package push --cascade --format json`.
///
/// Same shape as `command::package::pipeline::push::PushReport` — re-declared
/// here rather than shared because the archive push path's struct is private
/// to that module and the two push legs (archive bundle vs. env layers)
/// evolve independently (Two Hats: no shared refactor across the split).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct EnvPushReport {
    /// SHA-256 manifest digest of the pushed image. Captured for parity with
    /// the archive `PushReport` but not surfaced in run-summary.json.
    #[serde(default)]
    #[allow(dead_code)]
    pub manifest_digest: Option<String>,
    #[serde(default)]
    pub cascade_tags_written: Vec<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// Enumerate env manifests under `bundles_dir`: one per version directory
/// carrying an `env-manifest.json`, as written by
/// `prepare_env_version` (`{work_dir}/{version}/env-manifest.json`; the push
/// job's `bundles_dir` is that same `work_dir` for a pylock mirror).
///
/// W3.1 (deferred): manifest layer/metadata paths are absolute,
/// prepare-job-local paths — correct while prepare and push share one
/// `work_dir` (hermetic, local); CI-split path portability across separate
/// prepare/push jobs is a follow-up concern, not solved here.
pub(crate) async fn enumerate_env_manifests(bundles_dir: &Path) -> Result<Vec<EnvManifest>, String> {
    let mut manifests = Vec::new();

    let mut read_dir = tokio::fs::read_dir(bundles_dir)
        .await
        .map_err(|e| format!("failed to read bundles directory {}: {e}", bundles_dir.display()))?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| format!("failed to iterate bundles directory: {e}"))?
    {
        let manifest_path = entry.path().join("env-manifest.json");
        if !tokio::fs::try_exists(&manifest_path).await.unwrap_or(false) {
            continue;
        }

        let content = tokio::fs::read_to_string(&manifest_path)
            .await
            .map_err(|e| format!("failed to read {}: {e}", manifest_path.display()))?;
        let manifest: EnvManifest =
            serde_json::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", manifest_path.display()))?;
        manifests.push(manifest);
    }

    Ok(manifests)
}

/// Build the `ocx package push` argv for one env-package leg: `--cascade
/// --new` with the composed `metadata.json` via `-m` and the ordered wheel
/// layers as positional args. Pure and unit-testable — locks the multi-layer
/// + metadata invocation shape without spawning a subprocess.
pub(crate) fn build_env_push_args(
    platform: &str,
    target_ref: &str,
    metadata_path: &Path,
    layer_paths: &[PathBuf],
) -> Result<Vec<String>, String> {
    let metadata_str = metadata_path
        .to_str()
        .ok_or_else(|| format!("metadata path is not valid UTF-8: {}", metadata_path.display()))?;

    let mut args = vec![
        "--format".to_string(),
        "json".to_string(),
        "package".to_string(),
        "push".to_string(),
        "--cascade".to_string(),
        "--new".to_string(),
        "-p".to_string(),
        platform.to_string(),
        "-i".to_string(),
        target_ref.to_string(),
        "-m".to_string(),
        metadata_str.to_string(),
    ];

    for layer_path in layer_paths {
        let layer_str = layer_path
            .to_str()
            .ok_or_else(|| format!("layer path is not valid UTF-8: {}", layer_path.display()))?;
        args.push(layer_str.to_string());
    }

    Ok(args)
}

/// Invoke `ocx package push` for one env-package leg and parse the JSON
/// report. Mirrors `push::invoke_push`'s subprocess shape — binary
/// resolution + `OCX_*` env forwarding — with the multi-layer argv from
/// [`build_env_push_args`] instead of the archive path's single bundle file.
///
/// Returns a descriptive error string on subprocess failure (caller records
/// as `push_error` without aborting the run), matching `invoke_push`'s
/// contract.
pub(crate) async fn invoke_env_push(
    platform: &str,
    target_ref: &str,
    metadata_path: &Path,
    layer_paths: &[PathBuf],
) -> Result<EnvPushReport, String> {
    let args = build_env_push_args(platform, target_ref, metadata_path, layer_paths)?;

    let ocx_binary = crate::pipeline::ocx_cli::resolve_ocx_binary()?;
    let mut cmd = tokio::process::Command::new(&ocx_binary);
    cmd.args(&args);
    crate::pipeline::ocx_cli::forward_ocx_env(&mut cmd);

    let output = cmd.output().await.map_err(|e| format!("failed to spawn ocx: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ocx package push exited {}: {}", output.status, stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .map_err(|e| format!("failed to parse push JSON output: {e}\nstdout: {}", stdout.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_env_push_args_orders_flags_and_layers() {
        let layers = vec![
            PathBuf::from("/work/layers/pycowsay.tar.zst"),
            PathBuf::from("/work/layers/six.tar.zst"),
        ];
        let metadata_path = PathBuf::from("/work/metadata.json");

        let args = build_env_push_args("linux/amd64", "pycowsay:1.0.0", &metadata_path, &layers)
            .expect("valid UTF-8 paths build cleanly");

        assert!(args.contains(&"--cascade".to_string()));
        assert!(args.contains(&"--new".to_string()));

        let platform_flag = args.iter().position(|a| a == "-p").expect("-p flag present");
        assert_eq!(args[platform_flag + 1], "linux/amd64");

        let identifier_flag = args.iter().position(|a| a == "-i").expect("-i flag present");
        assert_eq!(args[identifier_flag + 1], "pycowsay:1.0.0");

        let metadata_flag = args.iter().position(|a| a == "-m").expect("-m flag present");
        assert_eq!(args[metadata_flag + 1], "/work/metadata.json");

        // Layers are ordered positionals appended after the flags, in input order.
        let layer_start = metadata_flag + 2;
        assert_eq!(args[layer_start], "/work/layers/pycowsay.tar.zst");
        assert_eq!(args[layer_start + 1], "/work/layers/six.tar.zst");
        assert_eq!(args.len(), layer_start + 2, "no trailing args beyond the two layers");
    }

    #[tokio::test]
    async fn enumerate_env_manifests_reads_per_version_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let version_dir = temp.path().join("1.0.0");
        tokio::fs::create_dir_all(&version_dir)
            .await
            .expect("create version dir");

        let manifest = EnvManifest {
            version: "1.0.0".to_string(),
            envs: vec![],
        };
        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        tokio::fs::write(version_dir.join("env-manifest.json"), json)
            .await
            .expect("write manifest");

        let manifests = enumerate_env_manifests(temp.path()).await.expect("reads manifest");
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].version, "1.0.0");
    }

    #[tokio::test]
    async fn enumerate_env_manifests_skips_directories_without_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        tokio::fs::create_dir_all(temp.path().join("not-a-version"))
            .await
            .expect("create dir");

        let manifests = enumerate_env_manifests(temp.path()).await.expect("empty ok");
        assert!(manifests.is_empty());
    }
}
