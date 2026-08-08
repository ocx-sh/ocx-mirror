// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Helpers shared by more than one `pipeline push` test module.

use super::super::*;
use crate::run_summary::LayerReuse;

/// Serialises every test that drives a push against the process env it
/// reads — `OCX_MIRROR_JOB_URL`, `OCX_BINARY_PIN` and the announce token.
///
/// Without it two stamping tests race: one removes `OCX_MIRROR_JOB_URL`
/// before the other's `push` reads it at startup, dropping the expected
/// stamp. `OCX_BINARY_PIN` is worse than a dropped stamp — [`invoke_push`]
/// resolves it per subprocess, so a test whose premise is "no `ocx` is
/// reachable" silently runs a *neighbouring* test's stand-in and merges its
/// platforms into that test's stand-in registry. That failed roughly one run
/// in twelve as a rolling alias carrying a version from another fixture.
///
/// Every test that drives a push must hold this, whether or not it touches
/// the environment itself — the tests that DO set these vars are the hazard
/// to the ones that merely assume a clean environment. `std::sync::Mutex` is
/// not reentrant, so it is taken by the test rather than by
/// [`run_push_cmd`]: the tests that mutate env need it across a wider span
/// (set → push → assert) and would deadlock against an inner acquisition.
///
// ponytail: a process-global lock, which is why these tests must all stay in
// one binary — moving any of them to `tests/` puts them in a separate process
// where the guard covers nothing. The lock exists only because
// `resolve_ocx_binary()` reads `OCX_BINARY_PIN` from the environment; thread
// the resolved path through as a parameter instead and both the lock and the
// same-binary constraint go away.
pub fn job_url_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    crate::test_support::ocx_env_lock()
}

pub fn run_push_cmd(
    spec: std::path::PathBuf,
    junit_dir: std::path::PathBuf,
    bundles_dir: std::path::PathBuf,
    summary_path: std::path::PathBuf,
) -> Result<(), MirrorError> {
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    let cmd = Push {
        spec,
        bundles_dir,
        junit_dir,
        write_summary: summary_path,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { cmd.execute(&printer).await })
}

/// Write a JUNIT file to a directory with canonical naming.
pub fn write_junit(dir: &std::path::Path, version: &str, platform_slug: &str, container_id: &str, xml: &str) {
    let name = format!("junit-{version}-{platform_slug}-{container_id}.xml");
    std::fs::write(dir.join(&name), xml).unwrap();
}

/// All-passing JUNIT for a (version, platform, container) triple.
pub fn passing_junit(version: &str, platform: &str, image: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}"
             tests="1" failures="0" errors="0" skipped="0"
             timestamp="2026-05-13T10:00:00Z" time="1.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="{platform}"/>
      <property name="ocx.image" value="{image}"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="1.0"/>
  </testsuite>
</testsuites>"#,
        slug = platform.replace('/', "_"),
        cid = image.replace([':', '/'], "_"),
    )
}

/// JUNIT with one failing test for a (version, platform, container) triple.
pub fn failing_junit(version: &str, platform: &str, image: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}"
             tests="1" failures="1" errors="0" skipped="0"
             timestamp="2026-05-13T10:00:00Z" time="2.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="{platform}"/>
      <property name="ocx.image" value="{image}"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="2.0">
      <failure message="exit code 1" type="exit_code">binary not found</failure>
    </testcase>
  </testsuite>
</testsuites>"#,
        slug = platform.replace('/', "_"),
        cid = image.replace([':', '/'], "_"),
    )
}

/// Write `{bundles_dir}/{version}/env-manifest.json` for the given
/// `(platform_slug, full platform key)` entries, each carrying `layers`
/// wheel layers.
pub fn write_env_manifest(bundles_dir: &Path, version: &str, entries: &[(&str, &str)], layers: &[&str]) -> PathBuf {
    use crate::pipeline::python_prepare::{EnvEntry, EnvManifest};

    let version_dir = bundles_dir.join(version);
    std::fs::create_dir_all(&version_dir).unwrap();

    let manifest = EnvManifest {
        version: version.to_string(),
        envs: entries
            .iter()
            .map(|(slug, platform)| EnvEntry {
                platform_slug: (*slug).to_string(),
                platform: (*platform).to_string(),
                metadata_path: PathBuf::from(format!("{slug}-metadata.json")),
                layers: layers.iter().map(|name| env_layer(name)).collect(),
            })
            .collect(),
    };
    std::fs::write(
        version_dir.join("env-manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    version_dir
}

pub fn version_summary(version: &str, status: VersionStatus, pushed: &[&str], tags: &[&str]) -> VersionSummary {
    VersionSummary {
        version: version.to_string(),
        status,
        platforms_pushed: pushed.iter().map(|s| (*s).to_string()).collect(),
        platforms_failed: vec![],
        cascade_tags_written: tags.iter().map(|s| (*s).to_string()).collect(),
        test_failures: vec![],
        platforms_excluded: vec![],
        layer_reuse: LayerReuse::default(),
    }
}

/// The `platform=version` entries the stand-in registry's `tag` index
/// carries, or empty when the tag was never written.
#[cfg(unix)]
pub fn tag_index(dir: &Path, tag: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join("tagstate").join(tag))
        .map(|body| body.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// A stand-in `ocx` that logs every invocation's argv and reports a push.
#[cfg(unix)]
pub fn fake_ocx_logging_push(dir: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ocx-log");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{log}'\necho '{{\"cascade_tags_written\":[],\"status\":\"pushed\"}}'\n",
            log = log.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A layer whose path is version-dir-RELATIVE, exactly as
/// `prepare_env_version` writes it (and as `enumerate_env_manifests` now
/// requires — an absolute or `..`-carrying path is refused).
pub fn env_layer(name: &str) -> crate::pipeline::python_prepare::EnvLayer {
    crate::pipeline::python_prepare::EnvLayer {
        wheel_repository: format!("pip-packages/example.com/{name}"),
        digest: format!("sha256:{}", "0".repeat(64)),
        path: PathBuf::from(format!("{name}.tar.zst")),
        package_name: name.to_string(),
        wheel_sha256: "1".repeat(64),
    }
}

/// Drive the whole push pipeline against a stand-in `ocx`.
#[cfg(unix)]
pub fn run_pipeline_with_fake_ocx(
    fixture: &str,
    script: &Path,
    junit_dir: &Path,
    bundles_dir: &Path,
    summary_path: &Path,
    token: Option<&str>,
) -> Result<(), MirrorError> {
    // SAFETY: test-only process env, serialised by the caller's lock.
    unsafe {
        std::env::set_var("OCX_BINARY_PIN", script);
        match token {
            Some(t) => std::env::set_var(ENV_ANNOUNCE_TOKEN, t),
            None => std::env::remove_var(ENV_ANNOUNCE_TOKEN),
        }
    }
    let result = run_push_cmd(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture),
        junit_dir.to_path_buf(),
        bundles_dir.to_path_buf(),
        summary_path.to_path_buf(),
    );
    // SAFETY: cleanup so neighbouring tests don't inherit either var.
    unsafe {
        std::env::remove_var("OCX_BINARY_PIN");
        std::env::remove_var(ENV_ANNOUNCE_TOKEN);
    }
    result
}

/// A stand-in `ocx` for the whole-pipeline tests: logs every argv, exits
/// `announce_exit` on `package announce`, and — crucially — models what a
/// push does to the *registry* rather than just answering with a canned
/// tag list.
///
/// The modelled semantics are `client.rs::merge_platform_into_index`: every
/// push merges its own platform into the exact version tag's index, and a
/// `--cascade` push additionally merges that **same single platform** into
/// each rolling tag, replacing only its own entry (`retain(|e| e.platform
/// != platform)`) and keeping every other platform's entry exactly as it
/// found it. A canned list that answered the same aliases for any
/// `--cascade` invocation could not observe that only one platform ever
/// reached them.
///
/// State lands in `{dir}/tagstate/{tag}`, one sorted `platform=version`
/// line per platform the tag's index carries — read back with [`tag_index`].
#[cfg(unix)]
pub fn fake_ocx_pipeline(dir: &Path, log: &Path, announce_exit: u8) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let state = dir.join("tagstate");
    std::fs::create_dir_all(&state).unwrap();
    let script = dir.join("fake-ocx");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"package announce"*)
    echo '{{"status":"updated","pull_request_url":"https://github.com/ocx-sh/index/pull/81"}}'
    exit {announce_exit}
    ;;
esac

# The push carries `-p PLATFORM` and `-i REPOSITORY:VERSION`.
platform=''; ref=''; prev=''
for a in "$@"; do
  case "$prev" in
    -p) platform="$a" ;;
    -i) ref="$a" ;;
  esac
  prev="$a"
done
version="${{ref##*:}}"
minor="${{version%.*}}"
major="${{minor%.*}}"

# merge_platform_into_index: read, drop THIS platform's entry, append it
# back pointing at this version, keep every other platform's entry.
merge() {{
  f='{state}'/"$1"
  [ -f "$f" ] || : > "$f"
  grep -v "^$platform=" "$f" > "$f.tmp"
  echo "$platform=$version" >> "$f.tmp"
  sort -o "$f" "$f.tmp"
  rm -f "$f.tmp"
}}

merge "$version"
case "$*" in
  *--cascade*)
    for t in "$minor" "$major" latest; do merge "$t"; done
    echo '{{"cascade_tags_written":["'"$minor"'","'"$major"'","latest"],"status":"pushed"}}'
    ;;
  *)
    echo '{{"cascade_tags_written":[],"status":"pushed"}}'
    ;;
esac
"#,
            log = log.display(),
            state = state.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Every logged `package push` argv that targets `repo:version`.
/// Pushes for `version`, matched on the **registry-qualified** `-i`.
///
/// The fixture targets ghcr.io, so that is what must reach the argv. A bare
/// `ocx-contrib/…` reference resolves against the default registry instead:
/// the first ghcr.io mirror sent five versions at `ocx.sh` and took
/// `403 UNAUTHORIZED: No permission to write manifest` on every one. Match
/// the whole reference so dropping the registry empties this list rather
/// than silently passing.
pub fn pushes_for(log: &str, version: &str) -> Vec<String> {
    log.lines()
        .filter(|line| line.contains("package push"))
        .filter(|line| line.contains(&format!("-i ghcr.io/ocx-contrib/bazelbuild/bazelisk:{version} ")))
        .map(str::to_string)
        .collect()
}
