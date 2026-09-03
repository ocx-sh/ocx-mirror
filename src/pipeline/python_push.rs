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

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use ocx_lib::log;
use ocx_lib::oci::Identifier;
use ocx_lib::publisher::Publisher;

use super::python_prepare::{EnvLayer, EnvManifest};
use crate::pipeline::ocx_cli::push::{PUSH_TIMEOUT, push_once};
use crate::pipeline::ocx_cli::sign::{ResolvedSign, sign_push_args};
use crate::pipeline::target_registry;
use crate::run_summary::LayerReuse;
use crate::spec::MirrorSpec;

/// Parsed JSON output from `ocx package push --cascade --format json`.
///
/// Same shape as `command::package::pipeline::push::PushReport` — re-declared
/// here rather than shared because the archive push path's struct is private
/// to that module and the two push legs (archive bundle vs. env layers)
/// evolve independently (Two Hats: no shared refactor across the split).
///
/// The report also carries the digest-named keep tags (`keep_tags_written` as
/// of ocx 0.6.0, `canonical_tags_written` before it). Neither name is captured
/// here, so the rename is inert — deliberately: the one
/// consumer of this struct's tag list is the announce union, which curates
/// human-meaningful version tags into the index — a canonical tag belongs in
/// neither the index entry nor a Discord release line.
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
///
/// Every declared path is checked for containment BEFORE the join (see
/// [`ensure_contained`]): the manifest arrives as a downloaded CI artifact, and
/// its paths become the `-m` and layer arguments of a real `ocx package push`.
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
        // download landed them at a different absolute location.
        let version_dir = entry.path();
        for env in &mut manifest.envs {
            ensure_contained(&env.metadata_path, &manifest_path, "metadata_path")?;
            env.metadata_path = version_dir.join(&env.metadata_path);
            for layer in &mut env.layers {
                ensure_contained(&layer.path, &manifest_path, "layer path")?;
                layer.path = version_dir.join(&layer.path);
            }
        }

        manifests.push(manifest);
    }

    Ok(manifests)
}

/// Refuse a manifest-declared path that would resolve outside its version
/// directory.
///
/// `Path::join` discards the base when the joined component is absolute, and
/// `..` walks out of it — so an `env-manifest.json` declaring either would
/// hand `ocx package push` a `-m` sidecar or a layer from anywhere on the
/// runner. The manifest is a downloaded CI artifact, i.e. input from outside
/// this process, so the containment is checked rather than assumed
/// (defence-in-depth: `prepare_env_version` only ever writes normal relative
/// components).
fn ensure_contained(path: &Path, manifest_path: &Path, field: &str) -> Result<(), String> {
    if path.is_absolute() || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!(
            "{} declares an out-of-tree {field} '{}': env manifest paths must be plain relative paths under the version directory",
            manifest_path.display(),
            path.display(),
        ));
    }
    Ok(())
}

/// Build the `ocx package push` argv for one env-package leg: `--cascade`
/// with the composed `metadata.json` via `-m` and the ordered wheel
/// layers as positional args, each carrying a `:from=<wheel_repository>` tail
/// so the push attempts a cross-repository blob mount against the wheel's
/// standalone registration (see [`register_wheel_layers`]) before falling
/// back to a full upload. Pure and unit-testable — locks the multi-layer +
/// metadata + mount-tail + annotation invocation shape without spawning a
/// subprocess.
///
/// `annotations` is the assembled `crate::annotations` map, rendered as the
/// same repeated `--annotation KEY=VALUE` tail the archive leg emits: an env
/// package is a published GHCR artifact like any other, and without
/// `image.source` GHCR cannot link it to the mirror repository.
pub(crate) fn build_env_push_args(
    platform: &str,
    target_ref: &str,
    metadata_path: &Path,
    layers: &[EnvLayer],
    annotations: &BTreeMap<String, String>,
    cascade: bool,
    sign: Option<&ResolvedSign>,
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
    // tags; the caller drops it for versions ocx cannot parse.
    if cascade {
        args.push("--cascade".to_string());
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

    args.extend(crate::annotations::push_args(annotations));
    args.extend(sign_push_args(sign));

    Ok(args)
}

/// Invoke `ocx package push` for one env-package leg and parse the JSON
/// report, retrying transient failures through the archive leg's ladder.
///
/// The argv is the multi-layer one from [`build_env_push_args`] instead of the
/// archive path's single bundle file; everything after that — attempt timeout,
/// the 75-retryable / 69-terminal exit-code split, the
/// `spec.concurrency.max_retries`-bounded jittered backoff, and the give-up
/// message naming that knob — is `push::push_with_retry`, so the two legs
/// cannot drift on what counts as retryable.
///
/// Returns a descriptive error string on subprocess failure (caller records
/// as `push_error` without aborting the run), matching `invoke_push`'s
/// contract.
#[expect(
    clippy::too_many_arguments,
    reason = "one env-push leg's full identity; same shape and same reasoning as the archive leg's `push_with_retry`"
)]
pub(crate) async fn invoke_env_push(
    spec: &MirrorSpec,
    platform: &str,
    target_ref: &str,
    metadata_path: &Path,
    layers: &[EnvLayer],
    annotations: &BTreeMap<String, String>,
    cascade: bool,
    sign: Option<&ResolvedSign>,
) -> Result<EnvPushReport, String> {
    let args = build_env_push_args(platform, target_ref, metadata_path, layers, annotations, cascade, sign)?;
    let ocx_binary = crate::pipeline::ocx_cli::resolve_ocx_binary()?;

    let report = crate::pipeline::ocx_cli::push::push_with_retry(
        &ocx_binary,
        &args,
        spec.concurrency.max_retries,
        &spec.name,
        target_ref,
        platform,
        sign,
    )
    .await?;

    Ok(EnvPushReport {
        manifest_digest: report.manifest_digest,
        cascade_tags_written: report.cascade_tags_written,
        status: report.status,
        layers: report.layers,
    })
}

/// Registers each not-yet-published wheel layer with the target registry so
/// the app's own push (the `:from=<wheel_repository>` tail built in
/// [`build_env_push_args`]) can cross-repository *mount* the blob instead of
/// re-uploading it (Decision D, shared wheel layers).
///
/// `registered` dedupes `wheel_repository:wheel_sha256` pairs across the
/// whole `pipeline push` run — once a wheel is confirmed present or pushed,
/// later legs sharing it (same wheel across platforms, or across app
/// versions) skip the registry round-trip. Only definitive outcomes enter
/// the set: after a transient check/push failure the next leg retries,
/// instead of one hiccup disabling that wheel's registration for the run.
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
    annotations: &BTreeMap<String, String>,
    registered: &mut HashSet<String>,
) {
    for layer in layers {
        let key = format!("{}:{}", layer.wheel_repository, layer.wheel_sha256);
        if registered.contains(&key) {
            continue;
        }

        match wheel_tag_exists(publisher, registry, &layer.wheel_repository, &layer.wheel_sha256).await {
            Ok(true) => {
                registered.insert(key);
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                log::warn!("wheel registration check failed for {key}: {e}");
                continue;
            }
        }

        if let Err(e) = push_wheel_layer(registry, platform, layer, annotations).await {
            log::warn!("wheel registration push failed for {key}: {e}");
        } else {
            registered.insert(key);
        }
    }
}

/// Checks whether `wheel_repository:wheel_sha256` is already published,
/// via the same fail-safe tag-listing helper `plan`/`sync` use (issue #157:
/// only an authoritative not-found counts as absent). A transient registry
/// error surfaces as `Err` and the caller skips this leg's registration —
/// warn-and-continue, never aborting; the app push falls back to a full
/// upload and a later leg retries the check.
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
///
/// It IS a published GHCR artifact, so it carries the run's annotations for
/// the same reason the app's own push does: `image.source` is what links a
/// package to its repository and inherits that repository's permissions.
///
/// Bounded by [`PUSH_TIMEOUT`] through [`push_once`] — a wedged child here
/// would otherwise hang the whole serial push driver on a step whose entire
/// purpose is to save an upload. Deliberately a SINGLE attempt (no retry
/// ladder): the caller warns and continues, and the app's own push falls back
/// to a full upload on a mount miss.
async fn push_wheel_layer(
    registry: &str,
    platform: &str,
    layer: &EnvLayer,
    annotations: &BTreeMap<String, String>,
) -> Result<(), String> {
    let metadata_path = write_wheel_registration_metadata(layer, platform).await?;
    let target_ref = format!("{registry}/{}:{}", layer.wheel_repository, layer.wheel_sha256);

    let layer_str = layer
        .path
        .to_str()
        .ok_or_else(|| format!("wheel layer path is not valid UTF-8: {}", layer.path.display()))?;
    let metadata_str = metadata_path
        .to_str()
        .ok_or_else(|| format!("wheel metadata path is not valid UTF-8: {}", metadata_path.display()))?;

    let mut args: Vec<String> = [
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
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();
    args.extend(crate::annotations::push_args(annotations));

    let ocx_binary = crate::pipeline::ocx_cli::resolve_ocx_binary()?;
    // Unsigned, deliberately: this push registers a wheel *layer* in the
    // shared `pip-packages/...` repository so the app's own push can mount it.
    // It is upload-avoidance, not a mirror-produced package — the signed
    // subjects are the app's platform manifests and their index.
    push_once(&ocx_binary, &args, PUSH_TIMEOUT, None)
        .await
        .map(|_report| ())
        .map_err(|failure| failure.message)
}

/// Writes a minimal Bundle metadata.json (version only, no env/entrypoints)
/// for a standalone wheel-registration push, NEXT TO the layer it registers —
/// i.e. inside this run's private work directory, never the shared OS temp
/// directory.
///
/// A predictable name under a world-writable `/tmp` is a symlink-clobber
/// invitation (CWE-377): any local user can pre-create the path as a symlink
/// and steer this write, and the file is the `-m` sidecar of a real publish.
/// The filename stays platform-qualified for clarity and debuggability even
/// though the content is not — the sidecar carries no `platform` key, so two
/// platforms of the same wheel now serialize byte-identically; only the
/// `sha256`-only name would otherwise fail to distinguish the two paths.
///
/// ponytail: no explicit cleanup — the layer directory is this run's own work
/// dir and goes away with it.
async fn write_wheel_registration_metadata(layer: &EnvLayer, platform: &str) -> Result<PathBuf, String> {
    use ocx_lib::package::metadata::{Metadata, bundle};

    // `binaries: None` — undeclared, not `Some([])`. This package exists only
    // as a cross-repo mount source; it is never installed, never composed, and
    // nothing ever scanned its tree, so it has no interface-binaries claim to
    // make either way.
    let metadata = Metadata::Bundle(bundle::Bundle {
        version: bundle::Version::V1,
        strip_components: None,
        env: Default::default(),
        dependencies: Default::default(),
        entrypoints: Default::default(),
        binaries: None,
        integrations: Default::default(),
    });
    let json = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("failed to serialize wheel registration metadata: {e}"))?;

    let directory = layer
        .path
        .parent()
        .ok_or_else(|| format!("wheel layer path has no parent directory: {}", layer.path.display()))?;
    let path = directory.join(format!(
        "ocx-mirror-wheel-{}-{}-metadata.json",
        layer.wheel_sha256,
        crate::spec::platform_key_slug(platform),
    ));
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

    fn annotations(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn build_env_push_args_orders_flags_and_layers() {
        let layers = vec![
            env_layer(
                "/work/layers/pycowsay.tar.zst",
                "pip-packages/files.pythonhosted.org/pycowsay",
                "aaa",
                "pycowsay",
            ),
            env_layer(
                "/work/layers/six.tar.zst",
                "pip-packages/files.pythonhosted.org/six",
                "bbb",
                "six",
            ),
        ];
        let metadata_path = PathBuf::from("/work/metadata.json");
        let empty = BTreeMap::new();

        let args = build_env_push_args(
            "linux/amd64",
            "pycowsay:1.0.0",
            &metadata_path,
            &layers,
            &empty,
            true,
            None,
        )
        .expect("valid UTF-8 paths build cleanly");

        assert!(args.contains(&"--cascade".to_string()));

        // cascade=false (a version ocx cannot parse) drops the flag but keeps
        // the identifier, metadata, and ordered layers.
        let no_cascade = build_env_push_args(
            "linux/amd64",
            "pycowsay:0.0.0.2",
            &metadata_path,
            &layers,
            &empty,
            false,
            None,
        )
        .expect("valid UTF-8 paths build cleanly");
        assert!(!no_cascade.contains(&"--cascade".to_string()));
        assert!(no_cascade.contains(&"pycowsay:0.0.0.2".to_string()));
        assert_eq!(
            no_cascade.iter().filter(|a| a.contains(".tar.zst:from=")).count(),
            2,
            "every layer positional carries a :from= mount tail"
        );

        let platform_flag = args.iter().position(|a| a == "-p").expect("-p flag present");
        assert_eq!(args[platform_flag + 1], "linux/amd64");

        // A full wheels key travels to `-p` verbatim — ocx parses the
        // `+libc.*` suffix into the index entry's `os.features`.
        let libc_args = build_env_push_args(
            "linux/amd64+libc.glibc",
            "pycowsay:1.0.0",
            &metadata_path,
            &layers,
            &empty,
            true,
            None,
        )
        .expect("valid UTF-8 paths build cleanly");
        let libc_flag = libc_args.iter().position(|a| a == "-p").expect("-p flag present");
        assert_eq!(libc_args[libc_flag + 1], "linux/amd64+libc.glibc");

        let identifier_flag = args.iter().position(|a| a == "-i").expect("-i flag present");
        assert_eq!(args[identifier_flag + 1], "pycowsay:1.0.0");

        let metadata_flag = args.iter().position(|a| a == "-m").expect("-m flag present");
        assert_eq!(args[metadata_flag + 1], "/work/metadata.json");

        // Layers are ordered positionals appended after the flags, in input
        // order, each `{path}:from={wheel_repository}`.
        let layer_start = metadata_flag + 2;
        assert_eq!(
            args[layer_start],
            "/work/layers/pycowsay.tar.zst:from=pip-packages/files.pythonhosted.org/pycowsay"
        );
        assert_eq!(
            args[layer_start + 1],
            "/work/layers/six.tar.zst:from=pip-packages/files.pythonhosted.org/six"
        );
        assert_eq!(args.len(), layer_start + 2, "no trailing args beyond the two layers");
    }

    #[test]
    fn build_env_push_args_carries_the_annotation_tail() {
        // Regression: the env leg published without `--annotation`, so every
        // env package landed in GHCR unlinked from the mirror repository while
        // the archive leg's images carried `image.source`.
        let layers = vec![env_layer(
            "/work/layers/pycowsay.tar.zst",
            "pip-packages/files.pythonhosted.org/pycowsay",
            "aaa",
            "pycowsay",
        )];
        let declared = annotations(&[
            ("org.opencontainers.image.revision", "a1b2c3d4"),
            (
                "org.opencontainers.image.source",
                "https://github.com/ocx-sh/mirror-pycowsay",
            ),
        ]);

        let args = build_env_push_args(
            "linux/amd64",
            "pycowsay:1.0.0",
            Path::new("/work/metadata.json"),
            &layers,
            &declared,
            true,
            None,
        )
        .expect("valid UTF-8 paths build cleanly");

        // Same wire form and key order as the archive leg's `--annotation` tail.
        let tail = &args[args.len() - 4..];
        assert_eq!(
            tail,
            [
                "--annotation",
                "org.opencontainers.image.revision=a1b2c3d4",
                "--annotation",
                "org.opencontainers.image.source=https://github.com/ocx-sh/mirror-pycowsay",
            ]
        );
        // The tail comes AFTER the layer positionals, never between them.
        assert!(args[args.len() - 5].ends_with(":from=pip-packages/files.pythonhosted.org/pycowsay"));
    }

    /// The env leg carries the same C-052 tail as the archive leg.
    ///
    /// Its own test rather than trusting the shared `sign_push_args`: the two
    /// argv builders are separate functions, and the env leg has already
    /// shipped once without the `--annotation` tail the archive leg had.
    #[test]
    fn build_env_push_args_carries_the_sign_tail() {
        use crate::spec::{KeylessConfig, Ref, SignConfig};

        let config = SignConfig {
            keyless: Some(KeylessConfig {
                fulcio: Some(Ref::Literal("http://localhost:5555".into())),
                rekor: Some(Ref::Literal("http://localhost:3000".into())),
                identity_token: None,
            }),
            key: None,
        };
        let sign = crate::pipeline::ocx_cli::sign::resolve_sign(&config, &|_| None, &|_| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no files"))
        })
        .expect("literal endpoints resolve without reading anything");

        let layers = vec![env_layer(
            "/work/layers/pycowsay.tar.zst",
            "pip-packages/files.pythonhosted.org/pycowsay",
            "aaa",
            "pycowsay",
        )];
        let args = build_env_push_args(
            "linux/amd64",
            "pycowsay:1.0.0",
            Path::new("/work/metadata.json"),
            &layers,
            &BTreeMap::new(),
            true,
            Some(&sign),
        )
        .expect("valid UTF-8 paths build cleanly");

        assert_eq!(
            &args[args.len() - 5..],
            [
                "--sign",
                "--fulcio-url",
                "http://localhost:5555",
                "--rekor-url",
                "http://localhost:3000",
            ],
        );
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

    /// Write a one-entry manifest whose `metadata_path` and layer path are
    /// exactly `declared`, and enumerate it.
    async fn enumerate_with_declared_path(temp: &Path, declared: &str) -> Result<Vec<EnvManifest>, String> {
        use crate::pipeline::python_prepare::EnvEntry;

        let version_dir = temp.join("1.0.0");
        tokio::fs::create_dir_all(&version_dir).await.expect("create dirs");

        let manifest = EnvManifest {
            version: "1.0.0".to_string(),
            envs: vec![EnvEntry {
                platform_slug: "linux_amd64".to_string(),
                platform: "linux/amd64".to_string(),
                metadata_path: PathBuf::from(declared),
                layers: vec![env_layer(declared, "pip-packages/example", "aa", "example")],
            }],
        };
        tokio::fs::write(
            version_dir.join("env-manifest.json"),
            serde_json::to_string(&manifest).expect("serialize"),
        )
        .await
        .expect("write manifest");

        enumerate_env_manifests(temp).await
    }

    #[tokio::test]
    async fn enumerate_env_manifests_rejects_paths_escaping_the_version_dir() {
        // The manifest is a downloaded CI artifact: a `..`-carrying or absolute
        // path in it would become the `-m` sidecar / layer argument of a real
        // `ocx package push`, reading from anywhere on the runner.
        for declared in ["../escape/metadata.json", "/etc/passwd"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let error = enumerate_with_declared_path(temp.path(), declared)
                .await
                .expect_err("an out-of-tree path must be refused");
            assert!(
                error.contains("out-of-tree") && error.contains("env-manifest.json"),
                "the error must name the offending manifest, got: {error}"
            );
        }
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
