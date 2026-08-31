// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use ocx_lib::cli::ExitCode;

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── Push retry (issue #50) ────────────────────────────────────────────

/// The single version `mirror-push-retry.yml` publishes in these tests.
const PUSH_RETRY_VERSION: &str = "3.7.0";

/// A stand-in `ocx` whose push fails its first `failures` invocations with
/// `exit_code` and succeeds afterwards. The attempt count lands in
/// `{dir}/push-attempts` — same stateful-counter shape as
/// [`fake_ocx_pipeline`]'s `tagstate/`, and the only way to tell one
/// attempt from four.
#[cfg(unix)]
fn fake_ocx_flaky_push(dir: &Path, failures: u32, exit_code: u8) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ocx");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
attempts=$(cat '{counter}' 2>/dev/null || echo 0)
attempts=$((attempts + 1))
echo "$attempts" > '{counter}'
if [ "$attempts" -le {failures} ]; then
  echo 'operation timed out' >&2
  exit {exit_code}
fi
echo '{{"cascade_tags_written":["{PUSH_RETRY_VERSION}"],"status":"pushed"}}'
"#,
            counter = dir.join("push-attempts").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// How many times [`fake_ocx_flaky_push`] was invoked.
#[cfg(unix)]
fn push_attempts(dir: &Path) -> u32 {
    std::fs::read_to_string(dir.join("push-attempts"))
        .map(|body| body.trim().parse().unwrap_or(0))
        .unwrap_or(0)
}

/// Stage one green single-platform version for the retry fixture.
#[cfg(unix)]
fn stage_push_retry_version(junit_dir: &Path, bundles_dir: &Path) {
    write_junit(
        junit_dir,
        PUSH_RETRY_VERSION,
        "linux_amd64",
        "_native_",
        &passing_junit(PUSH_RETRY_VERSION, "linux/amd64", "_native_"),
    );
    // Contents are irrelevant — the push subprocess is a stand-in.
    std::fs::write(
        bundles_dir.join(format!("bundle-{PUSH_RETRY_VERSION}-linux_amd64.tar.xz")),
        b"x",
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn a_transient_push_failure_is_retried_and_the_tile_still_lands() {
    // A registry 503 on the first attempt. The bundle is built, the tests
    // are green, and the only thing between the run and a published image
    // is one more request — reporting `push_error` here throws the whole
    // leg away over a blip that costs a second to ride out.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let script = fake_ocx_flaky_push(dir.path(), 1, ExitCode::TempFail as u8);

    stage_push_retry_version(junit_dir.path(), bundles_dir.path());

    run_pipeline_with_fake_ocx(
        "mirror-push-retry.yml",
        &script,
        junit_dir.path(),
        bundles_dir.path(),
        &summary_path,
        None,
    )
    .expect("a transient push failure must not fail the run");

    assert_eq!(push_attempts(dir.path()), 2, "the failed attempt must be retried once");

    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let version = &val["versions"][0];
    assert_eq!(version["version"], PUSH_RETRY_VERSION, "got: {version}");
    assert_eq!(version["status"], "published", "got: {version}");
    assert_eq!(
        version["platforms_pushed"],
        serde_json::json!(["linux/amd64"]),
        "got: {version}",
    );
    assert_eq!(version["platforms_failed"], serde_json::json!([]), "got: {version}");
}

#[cfg(unix)]
#[test]
fn push_retries_stop_at_the_spec_max_retries() {
    // Two assertions in one number: the ladder is bounded (a registry that
    // is down stays down — the run must not sit there forever), and its
    // length comes from the spec. The fixture sets `max_retries: 2`, a
    // value no plausible hardcoding produces: the default 3 would spend
    // four attempts, an off-by-one two.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let script = fake_ocx_flaky_push(dir.path(), 99, ExitCode::TempFail as u8);

    stage_push_retry_version(junit_dir.path(), bundles_dir.path());

    let result = run_pipeline_with_fake_ocx(
        "mirror-push-retry.yml",
        &script,
        junit_dir.path(),
        bundles_dir.path(),
        &summary_path,
        None,
    );
    assert!(result.is_err(), "an exhausted retry ladder must still fail the run");
    assert_eq!(
        push_attempts(dir.path()),
        3,
        "one attempt plus the two retries the spec grants",
    );

    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let version = &val["versions"][0];
    assert_eq!(version["platforms_failed"][0]["reason"], "push_error", "got: {version}");
}

#[cfg(unix)]
#[test]
fn a_non_transient_push_failure_is_not_retried() {
    // Guards the other side of the retry predicate rather than the bug:
    // a rejected manifest (65) is deterministic, and re-sending it three
    // more times only makes the run slower. Passes before the fix by
    // construction — it exists to fail a retry-everything implementation.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let script = fake_ocx_flaky_push(dir.path(), 99, ExitCode::DataError as u8);

    stage_push_retry_version(junit_dir.path(), bundles_dir.path());

    let result = run_pipeline_with_fake_ocx(
        "mirror-push-retry.yml",
        &script,
        junit_dir.path(),
        bundles_dir.path(),
        &summary_path,
        None,
    );
    assert!(result.is_err(), "a rejected push must fail the run");
    assert_eq!(push_attempts(dir.path()), 1, "a deterministic rejection is not retried");

    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let version = &val["versions"][0];
    assert_eq!(version["platforms_failed"][0]["reason"], "push_error", "got: {version}");
}

#[cfg(unix)]
#[test]
fn max_retries_zero_is_a_single_attempt() {
    // The documented floor: `0` opts out of retrying entirely. Nothing else
    // pins it — an off-by-one in the loop guard would quietly make it two,
    // and the spec that asked for none would get one anyway.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let script = fake_ocx_flaky_push(dir.path(), 99, ExitCode::TempFail as u8);

    let spec: MirrorSpec = serde_yaml_ng::from_str(
        r#"
name: minimal
target:
  registry: ocx.sh
  repository: minimal
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
concurrency:
  max_retries: 0
"#,
    )
    .unwrap();

    // SAFETY: test-only process env, serialised by the lock above.
    unsafe { std::env::set_var("OCX_BINARY_PIN", &script) };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(invoke_push(
        &spec,
        "linux/amd64",
        "ocx.sh/minimal:1.0.0",
        &dir.path().join("bundle-1.0.0-linux_amd64.tar.xz"),
        false,
    ));
    // SAFETY: cleanup so neighbouring tests don't inherit the pin.
    unsafe { std::env::remove_var("OCX_BINARY_PIN") };

    assert!(result.is_err(), "the attempt failed and no retry was granted");
    assert_eq!(push_attempts(dir.path()), 1, "`max_retries: 0` is one attempt, total");
}

#[cfg(unix)]
#[test]
fn a_red_platform_stops_the_run_from_moving_any_rolling_alias_in_the_registry() {
    // The scenario that defeats every announce-side filter. mirror-bazelisk
    // has been publishing for months, so the index entry ALREADY curates
    // `latest`, `1`, `1.20` and `1.20.0`. Tonight's run publishes 1.21.0
    // with darwin/arm64 red.
    //
    // `ocx package announce --tags-file` is additive AND re-observes every
    // tag the entry already carries — so withholding `latest` and `1` from
    // this run's union buys nothing: the announce re-fetches them from the
    // registry and re-commits whatever they point at now. The only place
    // the damage can be prevented is the registry: those aliases must never
    // be moved onto 1.21.0's linux-only index in the first place.
    //
    // So this asserts on the argv the registry-facing subprocess received,
    // not on what the union computed. 1.20.0 (whole) still cascades, once,
    // on its LAST platform — an earlier push failing mid-version must not
    // leave an alias on a half-assembled index either.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let log = dir.path().join("invocations.log");
    let script = fake_ocx_pipeline(dir.path(), &log, 0);

    // An established mirror: `latest` and `1` already resolve to 1.19.0 on
    // both platforms. This is what a cascade merges INTO, and what it
    // leaves behind for any platform it does not carry.
    for tag in ["latest", "1"] {
        std::fs::write(
            dir.path().join("tagstate").join(tag),
            "darwin/arm64=1.19.0\nlinux/amd64=1.19.0\n",
        )
        .unwrap();
    }

    for (version, slug, platform, green) in [
        ("1.20.0", "linux_amd64", "linux/amd64", true),
        ("1.20.0", "darwin_arm64", "darwin/arm64", true),
        ("1.21.0", "linux_amd64", "linux/amd64", true),
        ("1.21.0", "darwin_arm64", "darwin/arm64", false),
    ] {
        let xml = if green {
            passing_junit(version, platform, "_native_")
        } else {
            failing_junit(version, platform, "_native_")
        };
        write_junit(junit_dir.path(), version, slug, "_native_", &xml);
        std::fs::write(bundles_dir.path().join(format!("bundle-{version}-{slug}.tar.xz")), b"x").unwrap();
    }

    let result = run_pipeline_with_fake_ocx(
        "mirror-two-platform-announce.yml",
        &script,
        junit_dir.path(),
        bundles_dir.path(),
        &summary_path,
        Some("gh-token"),
    );
    assert!(result.is_err(), "a red platform must still fail the push job");

    let invocations = std::fs::read_to_string(&log).unwrap();

    // 1.21.0: the green linux leg publishes under the exact version tag,
    // and NOTHING in the run moves `latest` / `1` / `1.21`.
    let partial = pushes_for(&invocations, "1.21.0");
    assert_eq!(partial.len(), 1, "only the green platform pushes: {partial:?}");
    assert!(
        !partial.iter().any(|argv| argv.contains("--cascade")),
        "a version with a red platform must never cascade: {partial:?}",
    );

    // 1.20.0 is whole, so what the REGISTRY ends up holding is the claim:
    // every rolling alias must resolve to 1.20.0 on BOTH platforms. A
    // cascade push merges only its own platform, so cascading on one push
    // per version leaves each alias carrying that platform at 1.20.0 and
    // every other one still at 1.19.0 — a mixed-version index that freezes
    // half the users on the old release, on this run and every one after.
    let both_at_1_20_0 = vec!["darwin/arm64=1.20.0".to_string(), "linux/amd64=1.20.0".to_string()];
    for tag in ["1.20", "1", "latest"] {
        assert_eq!(
            tag_index(dir.path(), tag),
            both_at_1_20_0,
            "rolling tag `{tag}` must carry every platform of the whole version",
        );
    }
    assert_eq!(tag_index(dir.path(), "1.20.0"), both_at_1_20_0, "exact version tag");

    // The argv that produced it: every push of a whole version cascades.
    let whole = pushes_for(&invocations, "1.20.0");
    assert_eq!(whole.len(), 2, "both platforms push: {whole:?}");
    let cascading: Vec<&String> = whole.iter().filter(|argv| argv.contains("--cascade")).collect();
    assert_eq!(
        cascading.len(),
        whole.len(),
        "every push of a whole version must cascade: {whole:?}",
    );

    // 1.21.0 is partial: its green platform reaches the exact version tag
    // and nothing else — no alias, and no `1.21` conjured from a fresh
    // single-platform index.
    assert_eq!(tag_index(dir.path(), "1.21.0"), vec!["linux/amd64=1.21.0".to_string()]);
    assert!(
        tag_index(dir.path(), "1.21").is_empty(),
        "a partial version writes no `1.21`"
    );

    // And the summary reports the registry truthfully: the partial version
    // carries its version tag alone because nothing else was ever written.
    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let partial_version = val["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["version"] == "1.21.0")
        .unwrap();
    // The whole version's `cascade_tags_written` is the UNION over its
    // platforms, and the dedup is what keeps it a set: both platforms now
    // cascade, so both reports re-list the same hierarchy and an
    // un-deduped accumulation would read `1.20 1 latest 1.20 1 latest`.
    let whole_version = val["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["version"] == "1.20.0")
        .unwrap();
    assert_eq!(
        whole_version["cascade_tags_written"],
        serde_json::json!(["1.20.0", "1.20", "1", "latest"]),
        "got: {whole_version}",
    );

    assert_eq!(partial_version["status"], "partial", "got: {partial_version}");
    assert_eq!(
        partial_version["cascade_tags_written"],
        serde_json::json!(["1.21.0"]),
        "got: {partial_version}",
    );
    assert_eq!(
        val["announce"]["tags"],
        serde_json::json!(["1.20.0", "1.20", "1", "latest", "1.21.0"]),
        "got: {val}",
    );
}

#[cfg(unix)]
#[test]
fn a_run_killed_during_the_announce_does_not_read_as_a_mirror_that_never_opted_in() {
    // The hosted runner is reclaimed, or a maintainer cancels a backfill,
    // while the announce subprocess is in flight. The `if: always()` steps
    // still upload run-summary.json. With `announce: None` written first
    // that summary serialises with the key ABSENT — which `pipeline notify`
    // reads as "no `announce:` block at all". Twelve images live in GHCR,
    // the index knows about none of them, and the artifact says the mirror
    // does not announce.
    //
    // Observed from inside the announce subprocess: that is exactly the
    // window in which the kill lands.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let observed = dir.path().join("observed-announce-state.log");

    use std::os::unix::fs::PermissionsExt;
    let script = dir.path().join("fake-ocx");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"package announce"*)
    grep -A1 '"announce": {{' '{summary}' | grep -o '"status": "[a-z_]*"' >> '{observed}'
    echo '{{"status":"updated","pull_request_url":"https://github.com/ocx-sh/index/pull/81"}}'
    ;;
  *) echo '{{"cascade_tags_written":[],"status":"pushed"}}' ;;
esac
"#,
            summary = summary_path.display(),
            observed = observed.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let version = "3.7.0";
    write_junit(
        junit_dir.path(),
        version,
        "linux_amd64",
        "_native_",
        &passing_junit(version, "linux/amd64", "_native_"),
    );
    std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

    run_pipeline_with_fake_ocx(
        "mirror-ghcr-announce.yml",
        &script,
        junit_dir.path(),
        bundles_dir.path(),
        &summary_path,
        Some("gh-token"),
    )
    .expect("a fully green run must exit 0");

    assert_eq!(
        std::fs::read_to_string(&observed).unwrap().trim(),
        r#""status": "interrupted""#,
        "the durable summary must already name the announce as in flight",
    );

    // And the placeholder is replaced once the call returns.
    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert_eq!(val["announce"]["status"], "announced", "got: {val}");
}

#[cfg(unix)]
#[test]
fn a_failed_announce_fails_the_push_job() {
    // `OCX_ANNOUNCE_TOKEN` expires. Every push is green, so without this
    // the check stays green forever, no scheduled-run alert fires because
    // nothing failed, and the index drifts arbitrarily far behind the
    // registry. Same reasoning as `any_red`: the images ARE in the
    // registry, and the exit code is how a partial outcome reaches the
    // pipeline and the maintainer.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let log = dir.path().join("invocations.log");
    let script = fake_ocx_pipeline(dir.path(), &log, 70);

    let version = "3.7.0";
    write_junit(
        junit_dir.path(),
        version,
        "linux_amd64",
        "_native_",
        &passing_junit(version, "linux/amd64", "_native_"),
    );
    std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

    let result = run_pipeline_with_fake_ocx(
        "mirror-ghcr-announce.yml",
        &script,
        junit_dir.path(),
        bundles_dir.path(),
        &summary_path,
        Some("gh-token"),
    );

    let error = result.expect_err("a failed announce must fail the push job");
    assert!(
        format!("{error}").contains("index announce for bazelbuild/bazelisk failed"),
        "got: {error}",
    );

    // The announce is the ONLY reason: every push was green.
    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert_eq!(val["any_red"], false, "got: {val}");
    assert_eq!(val["announce"]["status"], "failed", "got: {val}");
}

#[cfg(unix)]
#[test]
fn a_missing_credential_leaves_the_push_job_green_but_visible() {
    // The counterpart to the test above: a mirror without the secret is a
    // valid configuration (forks, test repos), so it degrades rather than
    // failing — but it must stay legible in the summary, and from there in
    // the `announce` job output and the Index row.
    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let log = dir.path().join("invocations.log");
    let script = fake_ocx_pipeline(dir.path(), &log, 70);

    let version = "3.7.0";
    write_junit(
        junit_dir.path(),
        version,
        "linux_amd64",
        "_native_",
        &passing_junit(version, "linux/amd64", "_native_"),
    );
    std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

    run_pipeline_with_fake_ocx(
        "mirror-ghcr-announce.yml",
        &script,
        junit_dir.path(),
        bundles_dir.path(),
        &summary_path,
        None,
    )
    .expect("a missing announce secret must not fail the push job");

    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert_eq!(val["announce"]["status"], "skipped_no_credential", "got: {val}");
}

#[test]
fn run_summary_omits_announce_when_the_run_never_announced() {
    let _env_lock = job_url_env_lock();
    // `pipeline notify` reads this file; an absent announce must not
    // appear as a null field it has to special-case.
    let spec_path = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/mirror-minimal.yml"
    ))
    .to_path_buf();
    let dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");

    run_push_cmd(
        spec_path,
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        summary_path.clone(),
    )
    .unwrap();

    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert!(val.get("announce").is_none(), "got: {val}");
}
