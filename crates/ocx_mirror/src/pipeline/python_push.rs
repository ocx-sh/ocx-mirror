// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Env-package push phase for `source.type: pylock` mirrors.
//!
//! A parallel path to the archive/binary push loop in
//! `command::package::pipeline::push::Push::execute`: reads the
//! `env-manifest.json` per version written by
//! [`prepare_env_version`](super::python_prepare::prepare_env_version), then
//! pushes each green `(version, platform)` env package via
//! `ocx package push` with the ordered wheel layers as positional layer args
//! (each carrying a `:from=<wheel_repository>` cross-repo mount tail — see
//! [`build_env_push_args`]) and the composed `metadata.json` via `-m`.
//!
//! Shared wheel layers (Decision D): [`register_wheel_layers`] pushes each
//! not-yet-published wheel standalone to its content-addressed
//! `pip-packages/...:<wheel_sha256>` repository before the app's own push, so
//! the `from=` mount above has a source blob to reuse. The app's own push
//! still falls back to a full upload on a mount miss — that fallback is
//! load-bearing, not a regression.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ocx_lib::log;
use ocx_lib::oci::Identifier;
use ocx_lib::publisher::Publisher;

use super::python_prepare::{EnvLayer, EnvManifest};
use crate::command::package::target_registry;
use crate::run_summary::LayerReuse;

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
    /// Layer-push outcome counts (mounted/uploaded/verified). `#[serde(default)]`
    /// because an `ocx` binary built before the layer-mount bump omits the
    /// field entirely — this must not fail the parse.
    #[serde(default)]
    pub layers: LayerReuse,
}

/// Enumerate env manifests under `bundles_dir`: one per version directory
/// carrying an `env-manifest.json`, as written by
/// `prepare_env_version` (`{version_dir}/env-manifest.json`).
///
/// `prepare_env_version` records each manifest's layer/metadata paths relative
/// to its version directory, so this function re-anchors them against the
/// directory the manifest was actually found in. That makes the paths portable
/// across the CI prepare→push job split, where the artifact is downloaded to a
/// different absolute location than prepare wrote it.
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
        let mut manifest: EnvManifest =
            serde_json::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", manifest_path.display()))?;

        // Re-anchor the version-dir-relative layer/metadata paths onto this
        // manifest's actual directory so they resolve after a CI artifact
        // download landed them at a different absolute location. (An
        // already-absolute path from an older/local manifest is left as-is:
        // `Path::join` with an absolute component discards the base.)
        let version_dir = entry.path();
        for env in &mut manifest.envs {
            env.metadata_path = version_dir.join(&env.metadata_path);
            for layer in &mut env.layers {
                layer.path = version_dir.join(&layer.path);
            }
        }

        manifests.push(manifest);
    }

    Ok(manifests)
}

/// Build the `ocx package push` argv for one env-package leg: `--cascade
/// --new` with the composed `metadata.json` via `-m` and the ordered wheel
/// layers as positional args, each carrying a `:from=<wheel_repository>` tail
/// so the push attempts a cross-repository blob mount against the wheel's
/// standalone registration (see [`register_wheel_layers`]) before falling
/// back to a full upload. Pure and unit-testable — locks the multi-layer +
/// metadata + mount-tail invocation shape without spawning a subprocess.
pub(crate) fn build_env_push_args(
    platform: &str,
    target_ref: &str,
    metadata_path: &Path,
    layers: &[EnvLayer],
    cascade: bool,
) -> Result<Vec<String>, String> {
    let metadata_str = metadata_path
        .to_str()
        .ok_or_else(|| format!("metadata path is not valid UTF-8: {}", metadata_path.display()))?;

    let mut args = vec![
        "--format".to_string(),
        "json".to_string(),
        "package".to_string(),
        "push".to_string(),
    ];

    // `--cascade` requires an ocx-parseable X.Y.Z version to derive rolling
    // tags; the caller drops it for versions ocx cannot parse. `--new` only
    // matters alongside cascade (it treats the first-publish tag-list 404 as an
    // empty set), so the two travel together.
    if cascade {
        args.push("--cascade".to_string());
        args.push("--new".to_string());
    }

    args.extend([
        "-p".to_string(),
        platform.to_string(),
        "-i".to_string(),
        target_ref.to_string(),
        "-m".to_string(),
        metadata_str.to_string(),
    ]);

    for layer in layers {
        let layer_str = layer
            .path
            .to_str()
            .ok_or_else(|| format!("layer path is not valid UTF-8: {}", layer.path.display()))?;
        args.push(format!("{layer_str}:from={}", layer.wheel_repository));
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
    layers: &[EnvLayer],
    cascade: bool,
) -> Result<EnvPushReport, String> {
    let args = build_env_push_args(platform, target_ref, metadata_path, layers, cascade)?;

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

/// Registers each not-yet-published wheel layer with the target registry so
/// the app's own push (the `:from=<wheel_repository>` tail built in
/// [`build_env_push_args`]) can cross-repository *mount* the blob instead of
/// re-uploading it (Decision D, shared wheel layers).
///
/// `registered` dedupes `wheel_repository:wheel_sha256` pairs across the
/// whole `pipeline push` run — once a wheel has been checked or pushed once,
/// later legs sharing it (same wheel across platforms, or across app
/// versions) skip the registry round-trip.
///
/// Registration is pure upload-avoidance: every failure (the tag-exists
/// check or the push itself) is logged and skipped, never propagated. A miss
/// just means the app's own push falls back to a full upload — correct, only
/// not maximally efficient.
pub(crate) async fn register_wheel_layers(
    publisher: &Publisher,
    registry: &str,
    platform: &str,
    layers: &[EnvLayer],
    registered: &mut HashSet<String>,
) {
    for layer in layers {
        let key = format!("{}:{}", layer.wheel_repository, layer.wheel_sha256);
        if !registered.insert(key.clone()) {
            continue;
        }

        match wheel_tag_exists(publisher, registry, &layer.wheel_repository, &layer.wheel_sha256).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                log::warn!("wheel registration check failed for {key}: {e}");
                continue;
            }
        }

        if let Err(e) = push_wheel_layer(registry, platform, layer).await {
            log::warn!("wheel registration push failed for {key}: {e}");
        }
    }
}

/// Checks whether `wheel_repository:wheel_sha256` is already published,
/// via the same fail-safe tag-listing helper `plan`/`sync` use (issue #157:
/// only an authoritative not-found counts as absent, so a transient registry
/// error aborts the run rather than triggering a redundant re-push).
async fn wheel_tag_exists(
    publisher: &Publisher,
    registry: &str,
    wheel_repository: &str,
    wheel_sha256: &str,
) -> Result<bool, crate::error::MirrorError> {
    let identifier = Identifier::new_registry(wheel_repository, registry);
    let tags = target_registry::list_target_tags(publisher, &identifier).await?;
    Ok(tags.iter().any(|tag| tag == wheel_sha256))
}

/// Pushes one wheel layer standalone to `registry/wheel_repository:wheel_sha256`
/// with a minimal (version-only) Bundle metadata — no env/entrypoints, since
/// this content-addressed package exists only as a cross-repo mount source
/// and is never installed directly.
async fn push_wheel_layer(registry: &str, platform: &str, layer: &EnvLayer) -> Result<(), String> {
    let metadata_path = write_wheel_registration_metadata(&layer.wheel_sha256).await?;
    let target_ref = format!("{registry}/{}:{}", layer.wheel_repository, layer.wheel_sha256);

    let layer_str = layer
        .path
        .to_str()
        .ok_or_else(|| format!("wheel layer path is not valid UTF-8: {}", layer.path.display()))?;
    let metadata_str = metadata_path
        .to_str()
        .ok_or_else(|| format!("wheel metadata path is not valid UTF-8: {}", metadata_path.display()))?;

    let ocx_binary = crate::pipeline::ocx_cli::resolve_ocx_binary()?;
    let mut cmd = tokio::process::Command::new(&ocx_binary);
    cmd.args([
        "--format",
        "json",
        "package",
        "push",
        "-p",
        platform,
        "-i",
        target_ref.as_str(),
        "-m",
        metadata_str,
        layer_str,
    ]);
    crate::pipeline::ocx_cli::forward_ocx_env(&mut cmd);

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("failed to spawn ocx for wheel registration: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ocx package push (wheel registration) exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    Ok(())
}

/// Writes a minimal Bundle metadata.json (version only, no env/entrypoints)
/// for a standalone wheel-registration push, to a content-addressed path
/// under the OS temp directory. Every call for the same wheel writes
/// byte-identical content, so a concurrent overwrite race is harmless.
///
/// ponytail: no explicit cleanup of the written file — the OS temp directory
/// is reclaimed independently; add cleanup if temp-dir growth ever matters.
async fn write_wheel_registration_metadata(wheel_sha256: &str) -> Result<PathBuf, String> {
    use ocx_lib::package::metadata::{Metadata, bundle};

    let metadata = Metadata::Bundle(bundle::Bundle {
        version: bundle::Version::V1,
        strip_components: None,
        env: Default::default(),
        dependencies: Default::default(),
        entrypoints: Default::default(),
    });
    let json = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("failed to serialize wheel registration metadata: {e}"))?;

    let path = std::env::temp_dir().join(format!("ocx-mirror-wheel-{wheel_sha256}-metadata.json"));
    tokio::fs::write(&path, json)
        .await
        .map_err(|e| format!("failed to write wheel registration metadata to {}: {e}", path.display()))?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_layer(path: &str, wheel_repository: &str, wheel_sha256: &str, package_name: &str) -> EnvLayer {
        EnvLayer {
            wheel_repository: wheel_repository.to_string(),
            digest: format!("sha256:{wheel_sha256}"),
            path: PathBuf::from(path),
            package_name: package_name.to_string(),
            wheel_sha256: wheel_sha256.to_string(),
        }
    }

    #[test]
    fn build_env_push_args_orders_flags_and_layers() {
        let layers = vec![
            env_layer(
                "/work/layers/pycowsay.tar.zst",
                "pip-packages/files.pythonhosted.org/pycowsay/none-any",
                "aaa",
                "pycowsay",
            ),
            env_layer(
                "/work/layers/six.tar.zst",
                "pip-packages/files.pythonhosted.org/six/none-any",
                "bbb",
                "six",
            ),
        ];
        let metadata_path = PathBuf::from("/work/metadata.json");

        let args = build_env_push_args("linux/amd64", "pycowsay:1.0.0", &metadata_path, &layers, true)
            .expect("valid UTF-8 paths build cleanly");

        assert!(args.contains(&"--cascade".to_string()));
        assert!(args.contains(&"--new".to_string()));

        // cascade=false (a version ocx cannot parse) drops both flags but keeps
        // the identifier, metadata, and ordered layers.
        let no_cascade = build_env_push_args("linux/amd64", "pycowsay:0.0.0.2", &metadata_path, &layers, false)
            .expect("valid UTF-8 paths build cleanly");
        assert!(!no_cascade.contains(&"--cascade".to_string()));
        assert!(!no_cascade.contains(&"--new".to_string()));
        assert!(no_cascade.contains(&"pycowsay:0.0.0.2".to_string()));
        assert_eq!(
            no_cascade.iter().filter(|a| a.contains(".tar.zst:from=")).count(),
            2,
            "every layer positional carries a :from= mount tail"
        );

        let platform_flag = args.iter().position(|a| a == "-p").expect("-p flag present");
        assert_eq!(args[platform_flag + 1], "linux/amd64");

        let identifier_flag = args.iter().position(|a| a == "-i").expect("-i flag present");
        assert_eq!(args[identifier_flag + 1], "pycowsay:1.0.0");

        let metadata_flag = args.iter().position(|a| a == "-m").expect("-m flag present");
        assert_eq!(args[metadata_flag + 1], "/work/metadata.json");

        // Layers are ordered positionals appended after the flags, in input
        // order, each `{path}:from={wheel_repository}`.
        let layer_start = metadata_flag + 2;
        assert_eq!(
            args[layer_start],
            "/work/layers/pycowsay.tar.zst:from=pip-packages/files.pythonhosted.org/pycowsay/none-any"
        );
        assert_eq!(
            args[layer_start + 1],
            "/work/layers/six.tar.zst:from=pip-packages/files.pythonhosted.org/six/none-any"
        );
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
    async fn enumerate_env_manifests_resolves_relative_paths() {
        use crate::pipeline::python_prepare::{EnvEntry, EnvLayer};

        let temp = tempfile::tempdir().expect("tempdir");
        let version_dir = temp.path().join("1.0.0");
        let layers_dir = version_dir.join("linux_amd64/layers");
        tokio::fs::create_dir_all(&layers_dir).await.expect("create dirs");
        // The referenced files must exist so the resolved absolute paths do.
        tokio::fs::write(version_dir.join("linux_amd64/metadata.json"), "{}")
            .await
            .expect("write metadata");
        tokio::fs::write(layers_dir.join("wheel.tar.zst"), "x")
            .await
            .expect("write layer");

        // Manifest carries version-dir-relative paths, as prepare_env_version writes.
        let manifest = EnvManifest {
            version: "1.0.0".to_string(),
            envs: vec![EnvEntry {
                platform_slug: "linux_amd64".to_string(),
                platform: "linux/amd64".to_string(),
                variant: None,
                metadata_path: PathBuf::from("linux_amd64/metadata.json"),
                layers: vec![EnvLayer {
                    wheel_repository: "pip-packages/example".to_string(),
                    digest: "sha256:aaa".to_string(),
                    path: PathBuf::from("linux_amd64/layers/wheel.tar.zst"),
                    package_name: "example".to_string(),
                    wheel_sha256: "aa".to_string(),
                }],
            }],
        };
        tokio::fs::write(
            version_dir.join("env-manifest.json"),
            serde_json::to_string(&manifest).expect("serialize"),
        )
        .await
        .expect("write manifest");

        let manifests = enumerate_env_manifests(temp.path()).await.expect("enumerate");
        assert_eq!(manifests.len(), 1);
        let env = &manifests[0].envs[0];

        // Paths are re-anchored onto the version dir → absolute and existing.
        assert_eq!(env.metadata_path, version_dir.join("linux_amd64/metadata.json"));
        assert!(env.metadata_path.exists(), "resolved metadata path must exist");
        assert_eq!(env.layers[0].path, version_dir.join("linux_amd64/layers/wheel.tar.zst"));
        assert!(env.layers[0].path.exists(), "resolved layer path must exist");
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

    // ── EnvPushReport.layers — #[serde(default)] backward compat ──────────

    #[test]
    fn env_push_report_parses_layers_field_when_present() {
        let json = r#"{
            "manifest_digest": "sha256:aaa",
            "cascade_tags_written": ["1.0.0"],
            "status": "pushed",
            "layers": { "mounted": 2, "uploaded": 1, "verified": 3 }
        }"#;
        let report: EnvPushReport = serde_json::from_str(json).expect("parses");
        assert_eq!(report.layers.mounted, 2);
        assert_eq!(report.layers.uploaded, 1);
        assert_eq!(report.layers.verified, 3);
    }

    #[test]
    fn env_push_report_defaults_layers_when_absent() {
        // An `ocx` binary built before the layer-mount bump omits `layers`
        // entirely — the report must still parse, with all counts zero.
        let json = r#"{
            "manifest_digest": "sha256:aaa",
            "cascade_tags_written": ["1.0.0"],
            "status": "pushed"
        }"#;
        let report: EnvPushReport = serde_json::from_str(json).expect("parses without layers");
        assert_eq!(report.layers.mounted, 0);
        assert_eq!(report.layers.uploaded, 0);
        assert_eq!(report.layers.verified, 0);
    }
}
