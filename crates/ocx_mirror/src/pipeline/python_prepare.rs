// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Env-package prepare phase for `source.type: pylock` mirrors.
//!
//! A parallel path to the archive/binary [`orchestrator::prepare_version`] for
//! pylock-sourced specs: per `(version, platform, variant)` it downloads the
//! selected wheels, repacks each into a deterministic `tar.zst` layer, composes
//! the env-package metadata, and records everything in an `env-manifest.json`
//! so the push leg (W2.4) can materialize the multi-layer package with
//! `ocx package test`.
//!
//! The archive skeleton in `orchestrator` is left untouched — this module
//! mirrors its shape (two independent semaphores, `spawn_blocking` for the
//! CPU-bound repack, index-sorted deterministic output) rather than sharing it,
//! because a wheel-set env package emits N ordered layers + composed metadata,
//! not a single bundle.
//!
//! [`orchestrator::prepare_version`]: crate::pipeline::orchestrator::prepare_version

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ocx_lib::cli::progress::{ProgressManager, Spinner};
use ocx_lib::log;
use ocx_lib::oci::Platform;
use ocx_lib::package::metadata::dependency::Dependency;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::download;
use super::orchestrator::{ConcurrencyParams, task_dir};
use super::progress;
use crate::error::MirrorError;
use crate::spec::Target;

/// One wheel selected for an env package, resolved in the task builder where the
/// [`WheelRef`](ocx_python::WheelRef) is still in hand.
///
/// Several fields (`version`) are carried for the W2.4 push leg and the wheel
/// naming convention rather than consumed by W2.3 prepare.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `version` is threaded for the W2.4 push leg / wheel naming
pub(crate) struct SelectedWheel {
    /// The distribution name (e.g. `"numpy"`).
    pub package_name: String,
    /// The pinned wheel version (e.g. `"2.1.3"`).
    pub version: String,
    /// The wheel filename.
    pub filename: String,
    /// The concrete wheel download URL.
    pub url: url::Url,
    /// The wheel `sha256` (hex, no prefix) — verified before repack.
    pub sha256: String,
    /// Repo-relative wheel repository (`wheel_reference(scope, wheel).repository`).
    pub wheel_repository: String,
}

/// A single env-package work unit: download + repack + compose one
/// `(version, wheels key)`.
///
/// Several fields (`source_version`, `cascade`, `spec_dir`, `wheel_scope`) are
/// carried for the W2.4 push leg rather than consumed by W2.3 prepare.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields carried for the W2.4 push leg
pub(crate) struct WheelEnvTask {
    /// Bare normalized tag the pipeline publishes (e.g. `3.29.0` — env tags
    /// carry no variant prefix).
    pub normalized_version: String,
    /// Raw upstream (app) version, pre-tag.
    pub source_version: String,
    /// The full wheels-key OCX platform for this leg (`+libc.*` os_features
    /// intact — published verbatim as the image-index platform entry).
    pub platform: Platform,
    /// The publish target (registry + repository).
    pub target: Target,
    /// Whether cascade tags are published.
    pub cascade: bool,
    /// Directory the spec was loaded from.
    pub spec_dir: PathBuf,
    /// The wheels selected for this leg, in composition order.
    pub wheels: Vec<SelectedWheel>,
    /// The private interpreter dependency (python-build-standalone package,
    /// pinned by digest).
    pub interpreter: Dependency,
    /// Extras requested for this env (empty until W3 encodes per-app requests).
    pub requested_extras: Vec<String>,
    /// Extras the lock declares (its top-level `extras` key).
    pub declared_extras: Vec<String>,
    /// The selection target — the L2 platform encoding and ABI the wheels are
    /// checked against.
    pub python_target: ocx_python::PythonTarget,
    /// The maintainer-configured wheel repo scope.
    pub wheel_scope: ocx_python::WheelScope,
    /// Which wheels' console scripts synthesize as entrypoints — resolved by
    /// the caller (`PythonConfig::resolve_entrypoint_selection`) against this
    /// leg's app version before the task is built, so this stays a plain
    /// value here.
    pub entrypoint_selection: ocx_python::EntrypointSelection,
}

/// A repacked wheel layer recorded in the env manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EnvLayer {
    /// Repo-relative wheel repository (target-agnostic; push prepends the host).
    pub wheel_repository: String,
    /// The OCI digest of the layer (`sha256:…`).
    pub digest: String,
    /// Path to the written `tar.zst` layer.
    pub path: PathBuf,
    /// The distribution name the layer carries.
    pub package_name: String,
    /// The source wheel `sha256` (hex, no prefix).
    pub wheel_sha256: String,
}

/// One prepared env package: its metadata plus ordered wheel layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EnvEntry {
    /// BASE os/arch platform slug (e.g. `linux_amd64` — os_features dropped
    /// by `ascii_segments`). Names the JUnit files the push job gates on:
    /// CI matrix legs are keyed by base platform, so a `+libc.*` entry shares
    /// its base leg's slug.
    pub platform_slug: String,
    /// Full wheels-key platform string (e.g. `linux/amd64+libc.glibc`) —
    /// round-trips `+libc.*` and becomes the push `-p` value verbatim.
    pub platform: String,
    /// Path to the composed `metadata.json`.
    pub metadata_path: PathBuf,
    /// The ordered wheel layers.
    pub layers: Vec<EnvLayer>,
}

/// Per-version env manifest written to `{work_dir}/{version}/env-manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EnvManifest {
    /// The variant-prefixed normalized tag prepared.
    pub version: String,
    /// One entry per prepared `(platform, variant)`.
    pub envs: Vec<EnvEntry>,
}

/// Prepare every env-package leg for a single version: download, repack,
/// compose.
///
/// Runs legs concurrently with `max_downloads` and `max_bundles` semaphore
/// slots. Results are collected in completion order then sorted by input index
/// for deterministic output. On success writes
/// `{work_dir}/{version}/env-manifest.json` and returns the populated manifest.
///
/// # Errors
///
/// Propagates the first failing leg (download / verify / repack / collision /
/// compose) and any manifest I/O failure.
pub(crate) async fn prepare_env_version(
    version: &str,
    tasks: &[WheelEnvTask],
    work_dir: &Path,
    http_client: &reqwest::Client,
    concurrency: &ConcurrencyParams,
) -> Result<EnvManifest, MirrorError> {
    let download_sem = Arc::new(Semaphore::new(concurrency.max_downloads));
    let bundle_sem = Arc::new(Semaphore::new(concurrency.max_bundles));
    let progress = ProgressManager::hidden();

    let mut join_set = tokio::task::JoinSet::<(usize, Result<EnvEntry, MirrorError>)>::new();

    for (index, task) in tasks.iter().enumerate() {
        let task = task.clone();
        let task_dir = task_dir(work_dir, &task.normalized_version, &task.platform);
        let download_sem = download_sem.clone();
        let bundle_sem = bundle_sem.clone();
        let client = http_client.clone();
        let progress = progress.clone();

        join_set.spawn(async move {
            let spinner = progress.spinner(format!("{} {}", task.normalized_version, task.platform));
            let result = spinner
                .scope(prepare_env_task(
                    &task,
                    &task_dir,
                    &client,
                    &spinner,
                    &download_sem,
                    &bundle_sem,
                ))
                .await;
            (index, result)
        });
    }

    // Collect in completion order, then sort by index for deterministic output.
    let mut outcomes: Vec<(usize, Result<EnvEntry, MirrorError>)> = Vec::with_capacity(tasks.len());
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => {
                return Err(MirrorError::ExecutionFailed(vec![format!(
                    "env prepare task panicked: {e}"
                )]));
            }
        }
    }
    outcomes.sort_by_key(|(index, _)| *index);

    let mut envs = Vec::with_capacity(tasks.len());
    for (_, result) in outcomes {
        envs.push(result?);
    }

    let version_dir = work_dir.join(version);

    // Record layer/metadata paths relative to the version directory (the
    // manifest's own directory) rather than the absolute prepare-job-local
    // paths `prepare_env_task` returns. The push leg runs in a *separate* CI
    // job with this version dir downloaded to a different absolute location;
    // relative paths let `enumerate_env_manifests` re-anchor them against
    // wherever the artifact landed (see `python_push`).
    for env in &mut envs {
        env.metadata_path = relativize_to(&env.metadata_path, &version_dir);
        for layer in &mut env.layers {
            layer.path = relativize_to(&layer.path, &version_dir);
        }
    }

    let manifest = EnvManifest {
        version: version.to_owned(),
        envs,
    };

    tokio::fs::create_dir_all(&version_dir)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to create version dir: {e}")]))?;

    let manifest_path = version_dir.join("env-manifest.json");
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to serialize env manifest: {e}")]))?;
    tokio::fs::write(&manifest_path, json)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to write env-manifest.json: {e}")]))?;

    log::debug!("Wrote env manifest to {}", manifest_path.display());
    Ok(manifest)
}

/// Download, verify, repack, and compose a single env package.
///
/// The phase order is the load-bearing invariant (design D5): every wheel is
/// downloaded and its `sha256` verified **before** any repack, so a corrupt
/// download fails closed with no layer written. Then, under the bundle
/// semaphore, each wheel is repacked (CPU-bound → `spawn_blocking`), the set is
/// collision-checked, and `compose_env` produces the metadata written next to
/// the layers.
///
/// # Errors
///
/// Returns [`MirrorError::SourceError`] on a download failure,
/// [`MirrorError::PylockError`] on a `sha256` mismatch, collision, or compose
/// failure, and [`MirrorError::ExecutionFailed`] on a repack or filesystem
/// failure.
async fn prepare_env_task(
    task: &WheelEnvTask,
    task_dir: &Path,
    http_client: &reqwest::Client,
    spinner: &Spinner,
    download_sem: &Semaphore,
    bundle_sem: &Semaphore,
) -> Result<EnvEntry, MirrorError> {
    let wheels_dir = task_dir.join("wheels");
    let layers_dir = task_dir.join("layers");
    tokio::fs::create_dir_all(&wheels_dir)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to create wheels dir: {e}")]))?;

    // --- Download + verify phase (I/O-bound) ---
    // D5: verify every wheel's sha256 BEFORE any repack, so a corrupt download
    // fails closed with no layer written.
    {
        let _permit = download_sem.acquire().await.expect("semaphore closed");
        progress::set_stage(spinner, "Downloading", &task.normalized_version, &task.platform);

        for wheel in &task.wheels {
            // CWE-22 defense-in-depth: `wheel.filename` originates in the
            // committed `pylock.toml` (attacker-influenceable if a hostile lock
            // is mirrored) and `select_wheels` does not reject path separators
            // or `..`. Reject anything that is not a single plain path
            // component before it is joined into a write path.
            validate_wheel_filename(&wheel.filename)?;
            let wheel_path = wheels_dir.join(&wheel.filename);
            // Resume: an already-downloaded wheel is re-verified, not re-fetched.
            if !wheel_path.exists() {
                download::download(http_client, &wheel.url, &wheel_path)
                    .await
                    .map_err(|e| {
                        MirrorError::SourceError(format!("failed to download wheel '{}': {e:#}", wheel.filename))
                    })?;
            }

            let actual = file_sha256_hex(&wheel_path).await?;
            if !actual.eq_ignore_ascii_case(&wheel.sha256) {
                return Err(MirrorError::PylockError(format!(
                    "sha256 mismatch for wheel '{}': expected {}, got {actual}",
                    wheel.filename, wheel.sha256
                )));
            }
        }
    } // download permit released

    // --- Repack phase (CPU-bound) ---
    let repacked = {
        let _permit = bundle_sem.acquire().await.expect("semaphore closed");
        progress::set_stage(spinner, "Repacking", &task.normalized_version, &task.platform);

        let wheel_files: Vec<PathBuf> = task
            .wheels
            .iter()
            .map(|wheel| wheels_dir.join(&wheel.filename))
            .collect();
        let output_dir = layers_dir.clone();
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async {
                let mut repacked = Vec::with_capacity(wheel_files.len());
                for wheel_path in &wheel_files {
                    let layer = ocx_python::repack_wheel(wheel_path, &output_dir).await.map_err(|e| {
                        MirrorError::ExecutionFailed(vec![format!(
                            "failed to repack wheel {}: {e}",
                            wheel_path.display()
                        )])
                    })?;
                    repacked.push(layer);
                }
                Ok::<_, MirrorError>(repacked)
            })
        })
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("repack task panicked: {e}")]))??
    }; // bundle permit released

    // --- Collision pre-check ---
    ocx_python::check_collisions(&repacked).map_err(|e| MirrorError::PylockError(format!("wheel collision: {e}")))?;

    // --- Compose env metadata ---
    let spec = ocx_python::EnvSpec {
        requested_extras: task.requested_extras.clone(),
        declared_extras: task.declared_extras.clone(),
        interpreter: task.interpreter.clone(),
        target: task.python_target.clone(),
        entrypoint_selection: task.entrypoint_selection.clone(),
    };
    let composition = ocx_python::compose_env(&spec, &repacked)
        .map_err(|e| MirrorError::PylockError(format!("env composition failed: {e}")))?;

    // The tag identifier — the registry host enters here (D: single seam).
    let identifier = ocx_lib::oci::Identifier::new_registry(&task.target.repository, &task.target.registry)
        .clone_with_tag(&task.normalized_version);
    let info = composition.into_info(identifier);

    let metadata_path = task_dir.join("metadata.json");
    let metadata_json = serde_json::to_string_pretty(&info.metadata)
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to serialize metadata: {e}")]))?;
    tokio::fs::write(&metadata_path, metadata_json)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to write metadata.json: {e}")]))?;

    // repack wrote each layer under `layers_dir`; pair each with its wheel (same
    // order) to record the ordered layer set.
    let layers = task
        .wheels
        .iter()
        .zip(&repacked)
        .map(|(wheel, layer)| EnvLayer {
            wheel_repository: wheel.wheel_repository.clone(),
            digest: layer.layer_digest.clone(),
            path: layer.layer_path.clone(),
            package_name: wheel.package_name.clone(),
            wheel_sha256: wheel.sha256.clone(),
        })
        .collect();

    Ok(EnvEntry {
        // BASE slug: `ascii_segments` drops os_features, so a `+libc.*` entry
        // shares its base CI leg's JUnit naming (the work dir, by contrast,
        // is the os_features-aware `task_dir` slug).
        platform_slug: task.platform.ascii_segments().join("_"),
        platform: task.platform.to_string(),
        metadata_path,
        layers,
    })
}

/// Rejects a wheel filename that is not a single plain path component.
///
/// The filename comes from the committed `pylock.toml`, so it is untrusted
/// input at the filesystem boundary. Only a lone [`Component::Normal`] equal to
/// the input is accepted — this rejects `..`, `/`, `\`, absolute paths, `.`,
/// and empty names, all of which could otherwise escape the task's `wheels/`
/// directory once joined (CWE-22).
fn validate_wheel_filename(filename: &str) -> Result<(), MirrorError> {
    use std::path::Component;

    let mut components = Path::new(filename).components();
    let is_single_plain_component = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(name)), None) if name == std::ffi::OsStr::new(filename)
    );
    if is_single_plain_component {
        Ok(())
    } else {
        Err(MirrorError::PylockError(format!(
            "unsafe wheel filename '{filename}': must be a single path component"
        )))
    }
}

/// Return `path` made relative to `base` when it is a descendant; otherwise
/// return it unchanged. A non-descendant path (e.g. a named-variant leg living
/// in a sibling version dir) stays absolute so the push leg fails loudly
/// instead of silently re-anchoring it against the wrong root.
fn relativize_to(path: &Path, base: &Path) -> PathBuf {
    path.strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Computes the hex-encoded `sha256` of the file at `path`.
async fn file_sha256_hex(path: &Path) -> Result<String, MirrorError> {
    use sha2::{Digest, Sha256};

    let bytes = tokio::fs::read(path).await.map_err(|e| {
        MirrorError::ExecutionFailed(vec![format!("failed to read wheel for sha256 {}: {e}", path.display())])
    })?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `console_pkg` fixture wheel lives in the sibling `ocx_python` crate.
    /// A pure-Python (`none-any`) wheel with three `[console_scripts]` entries —
    /// two synthesize unconditionally, one is extras-gated on `d`.
    const FIXTURE_WHEEL: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../ocx_python/tests/fixtures/wheels/console_pkg-1.0.0-py3-none-any.whl"
    );
    const FIXTURE_FILENAME: &str = "console_pkg-1.0.0-py3-none-any.whl";

    fn interpreter_dependency() -> Dependency {
        let json = format!(r#"{{"identifier":"ocx.sh/cpython:3.13@sha256:{}"}}"#, "a".repeat(64));
        serde_json::from_str(&json).expect("interpreter dependency parses")
    }

    fn python_target() -> ocx_python::PythonTarget {
        ocx_python::PythonTarget {
            platform: ocx_python::TargetPlatform {
                operating_system: ocx_python::TargetOperatingSystem::Linux,
                architecture: ocx_python::TargetArchitecture::Amd64,
            },
            variant: ocx_python::VariantConstraints::default(),
            interpreter: ocx_python::InterpreterPin {
                python_version: "3.13".to_string(),
                python_full_version: "3.13.1".to_string(),
                abi: "cp313".to_string(),
                implementation: ocx_python::Implementation::CPython,
            },
        }
    }

    /// A one-wheel env task pinned to the `console_pkg` fixture with the given
    /// expected `sha256`.
    fn wheel_env_task(sha256: &str) -> WheelEnvTask {
        WheelEnvTask {
            normalized_version: "1.0.0".to_string(),
            source_version: "1.0.0".to_string(),
            platform: "linux/amd64".parse().expect("valid platform"),
            target: Target {
                registry: "ocx.sh".to_string(),
                repository: "acme-app".to_string(),
            },
            cascade: true,
            spec_dir: PathBuf::from("."),
            wheels: vec![SelectedWheel {
                package_name: "console-pkg".to_string(),
                version: "1.0.0".to_string(),
                filename: FIXTURE_FILENAME.to_string(),
                url: url::Url::parse("https://example.com/wheel.whl").expect("valid url"),
                sha256: sha256.to_string(),
                wheel_repository: "pip-packages/example.com/console-pkg".to_string(),
            }],
            interpreter: interpreter_dependency(),
            requested_extras: Vec::new(),
            declared_extras: Vec::new(),
            python_target: python_target(),
            wheel_scope: ocx_python::WheelScope::default(),
            // `All` preserves this fixture's existing single-wheel assertion
            // (its own "console-pkg" script must synthesize) unchanged — this
            // helper isn't exercising selection-mode resolution.
            entrypoint_selection: ocx_python::EntrypointSelection::All,
        }
    }

    /// Places the fixture wheel under `{task_dir}/wheels/{filename}` so the
    /// download step resumes (no network) and returns its real `sha256`.
    async fn place_fixture(task_dir: &Path) -> String {
        let wheels_dir = task_dir.join("wheels");
        tokio::fs::create_dir_all(&wheels_dir).await.expect("create wheels dir");
        let dest = wheels_dir.join(FIXTURE_FILENAME);
        tokio::fs::copy(FIXTURE_WHEEL, &dest).await.expect("copy fixture wheel");
        file_sha256_hex(&dest).await.expect("hash fixture wheel")
    }

    fn spinner() -> Spinner {
        ProgressManager::hidden().spinner("test".to_string())
    }

    /// Install the rustls crypto provider exactly once per process. Reqwest
    /// builds its TLS stack lazily on first `Client::new` and panics with
    /// "No provider set" if none is registered, even though these tests never
    /// hit the network (the wheel is pre-placed, so download is skipped).
    fn install_crypto_provider() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_env_task_writes_metadata_and_layers() {
        install_crypto_provider();
        let temp = tempfile::tempdir().expect("tempdir");
        let task_dir = temp.path().join("linux_amd64");
        let sha256 = place_fixture(&task_dir).await;

        let task = wheel_env_task(&sha256);
        let download_sem = Semaphore::new(1);
        let bundle_sem = Semaphore::new(1);
        let http_client = reqwest::Client::new();
        let spinner = spinner();

        let entry = prepare_env_task(&task, &task_dir, &http_client, &spinner, &download_sem, &bundle_sem)
            .await
            .expect("prepare_env_task succeeds");

        // metadata.json is a valid Bundle carrying the interpreter dep + synthesized entrypoints.
        let metadata_path = task_dir.join("metadata.json");
        assert!(metadata_path.exists(), "metadata.json must be written");
        let metadata: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&metadata_path).unwrap())
            .expect("metadata.json is valid JSON");
        assert_eq!(metadata["type"], "bundle");
        let deps = metadata["dependencies"].as_array().expect("dependencies present");
        assert!(
            deps.iter()
                .any(|dep| dep["identifier"].as_str().is_some_and(|id| id.contains("cpython"))),
            "interpreter dependency must be present: {metadata}"
        );
        let entrypoints = metadata["entrypoints"].as_object().expect("entrypoints present");
        assert!(
            entrypoints.contains_key("console-pkg"),
            "unconditional console script must synthesize: {metadata}"
        );

        // One ordered wheel layer, written under {task_dir}/layers/.
        assert_eq!(entry.layers.len(), 1, "one wheel -> one layer");
        let layer = &entry.layers[0];
        assert!(
            layer.path.exists(),
            "layer .tar.zst must exist at {}",
            layer.path.display()
        );
        assert!(
            layer.path.starts_with(task_dir.join("layers")),
            "layer must be written under the task's layers/ dir: {}",
            layer.path.display()
        );
        assert!(layer.digest.starts_with("sha256:"), "layer digest is an OCI digest");
        assert_eq!(layer.wheel_sha256, sha256, "layer records the source wheel sha256");
        assert_eq!(layer.package_name, "console-pkg");
        assert_eq!(entry.platform, "linux/amd64");
        assert_eq!(entry.platform_slug, "linux_amd64");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_env_task_rejects_sha256_mismatch() {
        install_crypto_provider();
        let temp = tempfile::tempdir().expect("tempdir");
        let task_dir = temp.path().join("linux_amd64");
        let _real_sha256 = place_fixture(&task_dir).await;

        // A wrong expected sha256 must fail verification BEFORE any repack.
        let task = wheel_env_task(&"0".repeat(64));
        let download_sem = Semaphore::new(1);
        let bundle_sem = Semaphore::new(1);
        let http_client = reqwest::Client::new();
        let spinner = spinner();

        let error = prepare_env_task(&task, &task_dir, &http_client, &spinner, &download_sem, &bundle_sem)
            .await
            .expect_err("a corrupt wheel must fail closed");

        assert!(matches!(error, MirrorError::PylockError(_)), "got {error:?}");
        assert_eq!(error.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
        assert!(
            !task_dir.join("layers").exists(),
            "no layer may be written when verification fails (verify precedes repack)"
        );
    }

    #[test]
    fn validate_wheel_filename_accepts_plain_and_rejects_escapes() {
        // A normal wheel filename is a single path component.
        assert!(validate_wheel_filename("numpy-2.1.3-cp313-cp313-manylinux_2_28_x86_64.whl").is_ok());
        assert!(validate_wheel_filename("pycowsay-1.0.0-py3-none-any.whl").is_ok());

        // Anything that could escape the wheels/ directory is rejected (CWE-22).
        for hostile in [
            "../evil.whl",
            "../../etc/passwd",
            "a/b.whl",
            "/abs/evil.whl",
            ".",
            "..",
            "",
            "sub/../x.whl",
        ] {
            let error = validate_wheel_filename(hostile).expect_err(&format!("must reject '{hostile}'"));
            assert!(
                matches!(error, MirrorError::PylockError(_)),
                "got {error:?} for '{hostile}'"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_env_task_rejects_path_traversal_filename() {
        install_crypto_provider();
        let temp = tempfile::tempdir().expect("tempdir");
        let task_dir = temp.path().join("linux_amd64");

        // A hostile lock names a wheel that escapes the task dir. Verify fails
        // closed at the filename boundary — before any download or repack.
        let mut task = wheel_env_task(&"0".repeat(64));
        task.wheels[0].filename = "../escape.whl".to_string();

        let download_sem = Semaphore::new(1);
        let bundle_sem = Semaphore::new(1);
        let http_client = reqwest::Client::new();
        let spinner = spinner();

        let error = prepare_env_task(&task, &task_dir, &http_client, &spinner, &download_sem, &bundle_sem)
            .await
            .expect_err("a path-traversal wheel filename must be rejected");
        assert!(matches!(error, MirrorError::PylockError(_)), "got {error:?}");
        assert!(
            !temp.path().join("escape.whl").exists(),
            "no file may be written outside the task's wheels/ dir"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_env_version_preserves_input_order() {
        // The determinism invariant: manifest env order follows the input task
        // order, NOT JoinSet completion order (which is nondeterministic).
        install_crypto_provider();
        let temp = tempfile::tempdir().expect("tempdir");
        let work_dir = temp.path();
        let version = "1.0.0";

        let platforms = ["linux/amd64", "linux/arm64"];
        let mut tasks = Vec::new();
        for platform_str in platforms {
            let platform: Platform = platform_str.parse().expect("valid platform");
            let leg_dir = task_dir(work_dir, version, &platform);
            let sha256 = place_fixture(&leg_dir).await;
            let mut task = wheel_env_task(&sha256);
            task.platform = platform;
            tasks.push(task);
        }

        let concurrency = ConcurrencyParams {
            max_downloads: 4,
            max_bundles: 4,
            compression_threads: 1,
        };
        let http_client = reqwest::Client::new();
        let manifest = prepare_env_version(version, &tasks, work_dir, &http_client, &concurrency)
            .await
            .expect("prepare_env_version succeeds");

        let ordered: Vec<&str> = manifest.envs.iter().map(|env| env.platform.as_str()).collect();
        assert_eq!(
            ordered, platforms,
            "manifest env order must follow input task order, not completion order"
        );
        assert!(
            work_dir.join(version).join("env-manifest.json").exists(),
            "env-manifest.json must be persisted"
        );

        // Portability: manifest paths are recorded relative to the version dir
        // (so they survive the CI prepare→push job split — see python_push).
        for env in &manifest.envs {
            assert!(
                env.metadata_path.is_relative(),
                "metadata_path must be version-dir-relative, got {}",
                env.metadata_path.display()
            );
            assert_eq!(env.metadata_path, Path::new(&env.platform_slug).join("metadata.json"));
            for layer in &env.layers {
                assert!(
                    layer.path.is_relative(),
                    "layer path must be version-dir-relative, got {}",
                    layer.path.display()
                );
                assert!(
                    layer.path.starts_with(&env.platform_slug),
                    "layer path must sit under the platform slug dir, got {}",
                    layer.path.display()
                );
            }
        }
    }
}
