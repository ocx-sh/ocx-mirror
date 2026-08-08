// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

// ── W2.2: pylock source — plan-phase wheel selection ────────────────────
//
// `build_pylock_plan_entries` is the registry-independent half of the
// pylock branch (the caller already fetched `all_tags`/`version_map` from
// the target registry) — the seam that reuses `select_wheels` instead of
// the regex `resolve_assets` (D1). Tested directly so no live OCI
// registry is needed; `pipeline plan`'s registry-facing prelude is
// unchanged for every source type.

fn pylock_fixture_spec_path() -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pylock.yml"))
}

#[tokio::test]
async fn build_pylock_plan_entries_emits_wheel_assets_per_platform() {
    let spec_path = pylock_fixture_spec_path();
    let spec = spec::load_spec(&spec_path)
        .await
        .expect("fixture spec must load and validate");
    let spec_dir = spec_path.parent().unwrap();

    let upstream_versions = list_upstream_versions(&spec, spec_dir)
        .await
        .expect("pylock source must list the app's locked version");
    assert_eq!(upstream_versions.len(), 1);
    assert_eq!(upstream_versions[0].version, "1.0.0");

    let Source::Pylock { path, .. } = &spec.source else {
        panic!("fixture spec must be source.type: pylock");
    };

    let version_map = VersionPlatformMap::default();
    let entries = build_pylock_plan_entries(&spec, spec_dir, path, &upstream_versions, &[], &version_map, &None)
        .await
        .expect("wheel selection must succeed for the fixture lock");

    assert_eq!(entries.len(), 1, "one declared (unnamed default) variant -> one entry");
    let entry = &entries[0];
    assert_eq!(
        entry.version, "1.0.0",
        "unnamed default variant must produce a bare tag"
    );
    assert_eq!(entry.source_version, "1.0.0");
    assert_eq!(entry.variant, None);
    assert!(matches!(entry.kind, PlanVersionKind::New));

    let mut platforms = entry.platforms.clone();
    platforms.sort();
    assert_eq!(platforms, vec!["linux/amd64".to_string(), "linux/arm64".to_string()]);

    // Two pure-python ("none-any") wheels apply identically on both
    // declared platforms -> N=2 wheel `PlanAssetEntry` per platform.
    assert_eq!(entry.assets.len(), 4, "2 wheels x 2 platforms");
    for platform in ["linux/amd64", "linux/arm64"] {
        let names: Vec<&str> = entry
            .assets
            .iter()
            .filter(|asset| asset.platform == platform)
            .map(|asset| asset.asset_name.as_str())
            .collect();
        assert_eq!(names.len(), 2, "platform {platform} must carry 2 wheel assets");
        assert!(names.contains(&"pycowsay-1.0.0-py3-none-any.whl"));
        assert!(names.contains(&"six-1.16.0-py2.py3-none-any.whl"));
    }

    // Wheel URLs are concrete absolute http(s) — the existing download
    // path (pipeline/download.rs) consumes them as-is.
    for asset in &entry.assets {
        assert_eq!(asset.url.scheme(), "https");
    }
}

#[tokio::test]
async fn build_pylock_plan_entries_skips_already_published_platforms() {
    let spec_path = pylock_fixture_spec_path();
    let spec = spec::load_spec(&spec_path)
        .await
        .expect("fixture spec must load and validate");
    let spec_dir = spec_path.parent().unwrap();

    let upstream_versions = list_upstream_versions(&spec, spec_dir).await.unwrap();
    let Source::Pylock { path, .. } = &spec.source else {
        panic!("fixture spec must be source.type: pylock");
    };

    // Both declared platforms already published for this version — a
    // repeat `pipeline plan` run must report no outstanding work.
    let mut version_map = VersionPlatformMap::default();
    let version = Version::parse("1.0.0").unwrap();
    version_map.add(version.clone(), "linux/amd64".parse().unwrap());
    version_map.add(version, "linux/arm64".parse().unwrap());

    let entries = build_pylock_plan_entries(&spec, spec_dir, path, &upstream_versions, &[], &version_map, &None)
        .await
        .unwrap();
    assert!(
        entries.is_empty(),
        "already-published (version, platform) pairs must be dropped"
    );
}

#[tokio::test]
async fn build_pylock_plan_entries_wraps_select_error_as_pylock_error_exit_65() {
    // A wheel with no tag intersecting the target platform (windows-only
    // build, no marker, requested against linux/amd64) is
    // `SelectError::NoCompatibleWheel` inside `ocx_python::select_wheels`
    // — must surface as `MirrorError::PylockError` (DataError, exit 65),
    // not panic or an unrelated error kind.
    let dir = tempfile::tempdir().unwrap();
    let lock_toml = r#"
lock-version = "1.0"

[[packages]]
name = "windows-only-pkg"
version = "1.0.0"

[[packages.wheels]]
name = "windows_only_pkg-1.0.0-cp313-cp313-win_amd64.whl"
url = "https://example.com/windows_only_pkg-1.0.0-cp313-cp313-win_amd64.whl"
hashes = { sha256 = "3333333333333333333333333333333333333333333333333333333333cccc" }
"#;
    tokio::fs::write(dir.path().join("pylock.toml"), lock_toml)
        .await
        .unwrap();

    let spec_yaml = r#"
name: windows-only-pkg
target:
  registry: ocx.sh
  repository: windows-only-pkg
source:
  type: pylock
  path: pylock.toml
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
    let spec_path = dir.path().join("mirror.yml");
    tokio::fs::write(&spec_path, spec_yaml).await.unwrap();
    let spec = spec::load_spec(&spec_path)
        .await
        .expect("fixture spec must load and validate");

    let upstream_versions = list_upstream_versions(&spec, dir.path()).await.unwrap();
    let version_map = VersionPlatformMap::default();

    let err = build_pylock_plan_entries(
        &spec,
        dir.path(),
        "pylock.toml",
        &upstream_versions,
        &[],
        &version_map,
        &None,
    )
    .await
    .expect_err("a windows-only wheel must fail selection for a linux/amd64 target");

    assert!(matches!(err, MirrorError::PylockError(_)), "got: {err:?}");
    assert_eq!(err.kind_exit_code(), ocx_lib::cli::ExitCode::DataError);
}

#[tokio::test]
async fn build_pylock_plan_entries_accepts_pep440_version_beyond_three_components() {
    // Regression (W3.2 first-green-loop blocker): a PyPI app version with
    // more than three numeric components — pycowsay's real `0.0.0.2`, or a
    // calendar version like `2024.1.1.1` — is a valid PEP 440 string but is
    // NOT a parseable `ocx_lib::Version` (a ≤3-component tool-release-tag
    // semver parser). The plan phase must not panic on it: an unparseable
    // tag cannot be in the `Version`-keyed publish map, so it is simply
    // treated as outstanding work.
    let dir = tempfile::tempdir().unwrap();
    let lock_toml = r#"
lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "0.0.0.2"

[[packages.wheels]]
name = "pycowsay-0.0.0.2-py3-none-any.whl"
url = "https://example.com/pycowsay-0.0.0.2-py3-none-any.whl"
hashes = { sha256 = "5c03d8a9c7666ec102aaed4bbd6c7d35228489ce236f95f6e5d079529c6a5050" }
"#;
    tokio::fs::write(dir.path().join("pylock.toml"), lock_toml)
        .await
        .unwrap();

    let spec_yaml = r#"
name: pycowsay
target:
  registry: dev.ocx.sh
  repository: ocx/pycowsay
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/cpython:3.13.1"
wheels:
  linux/amd64: ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;
    let spec_path = dir.path().join("mirror.yml");
    tokio::fs::write(&spec_path, spec_yaml).await.unwrap();
    let spec = spec::load_spec(&spec_path)
        .await
        .expect("fixture spec must load and validate");

    let upstream_versions = list_upstream_versions(&spec, dir.path()).await.unwrap();
    assert_eq!(upstream_versions[0].version, "0.0.0.2");

    let version_map = VersionPlatformMap::default();
    let entries = build_pylock_plan_entries(
        &spec,
        dir.path(),
        "pylock.toml",
        &upstream_versions,
        &[],
        &version_map,
        &None,
    )
    .await
    .expect("a >3-component PEP 440 version must plan without panicking");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].version, "0.0.0.2");
    assert_eq!(entries[0].platforms, vec!["linux/amd64".to_string()]);
    assert!(matches!(entries[0].kind, PlanVersionKind::New));
    assert_eq!(entries[0].assets.len(), 1, "one pure-python wheel -> one asset");
}

// ── dual-libc wheels keys: one entry, full keys in assets ────────────────

const DUAL_LIBC_LOCK: &str = r#"
lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"
url = "https://example.com/pycowsay-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"
hashes = { sha256 = "aaaa" }

[[packages.wheels]]
name = "pycowsay-1.0.0-cp313-cp313-musllinux_1_2_x86_64.whl"
url = "https://example.com/pycowsay-1.0.0-cp313-cp313-musllinux_1_2_x86_64.whl"
hashes = { sha256 = "bbbb" }
"#;

fn dual_libc_spec_with(build_timestamp: &str) -> MirrorSpec {
    let yaml = format!(
        r#"
name: pycowsay
target:
  registry: ocx.sh
  repository: pycowsay
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
build_timestamp: {build_timestamp}
wheels:
  "linux/amd64+libc.glibc": ~
  "linux/amd64+libc.musl": ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#
    );
    serde_yaml_ng::from_str(&yaml).unwrap()
}

/// The wheels-selection fixture, stamp-free — these tests assert on the tag's
/// shape (no variant prefix, one entry per version), not on stamping.
fn dual_libc_spec() -> MirrorSpec {
    dual_libc_spec_with("none")
}

#[test]
fn build_env_plan_entries_dual_libc_keys_share_one_entry_and_base_platform() {
    let spec = dual_libc_spec();
    let lock = ocx_python::parse_pylock(DUAL_LIBC_LOCK).unwrap();
    let version_map = VersionPlatformMap::default();

    let entries = build_env_plan_entries(&spec, &lock, "1.0.0", &[], &version_map, &None).unwrap();

    assert_eq!(entries.len(), 1, "env sources emit ONE bare-tag entry");
    let entry = &entries[0];
    assert_eq!(entry.version, "1.0.0", "bare tag, no variant prefix");
    assert_eq!(entry.variant, None);
    assert_eq!(
        entry.platforms,
        vec!["linux/amd64".to_string()],
        "platforms dedupes full keys onto the base CI matrix leg"
    );

    // Each full key selected its libc's wheel; assets carry the FULL key.
    let glibc: Vec<&str> = entry
        .assets
        .iter()
        .filter(|asset| asset.platform == "linux/amd64+libc.glibc")
        .map(|asset| asset.asset_name.as_str())
        .collect();
    assert_eq!(glibc, vec!["pycowsay-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"]);
    let musl: Vec<&str> = entry
        .assets
        .iter()
        .filter(|asset| asset.platform == "linux/amd64+libc.musl")
        .map(|asset| asset.asset_name.as_str())
        .collect();
    assert_eq!(musl, vec!["pycowsay-1.0.0-cp313-cp313-musllinux_1_2_x86_64.whl"]);
}

#[test]
fn build_env_plan_entries_published_dedup_honors_os_features() {
    // The glibc key is already published — only the musl key remains
    // outstanding; the published sibling must NOT mask it.
    let spec = dual_libc_spec();
    let lock = ocx_python::parse_pylock(DUAL_LIBC_LOCK).unwrap();
    let mut version_map = VersionPlatformMap::default();
    version_map.add(
        Version::parse("1.0.0").unwrap(),
        "linux/amd64+libc.glibc".parse().unwrap(),
    );

    let entries = build_env_plan_entries(&spec, &lock, "1.0.0", &["1.0.0".to_string()], &version_map, &None).unwrap();

    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.platforms, vec!["linux/amd64".to_string()]);
    assert_eq!(entry.assets.len(), 1, "only the musl key's wheel is planned");
    assert_eq!(entry.assets[0].platform, "linux/amd64+libc.musl");
}

// ── build_timestamp on the env path ──────────────────────────────────────

#[test]
fn build_env_plan_entries_stamps_the_tag_when_build_timestamp_is_configured() {
    // F2: `plan` computed the run's build timestamp but the env branches
    // never received it, so an env mirror that configured a stamp published
    // the bare `X.Y.Z` tag and re-pointed it (plus its whole cascade) on
    // every re-publish — the exact GC hazard
    // `MirrorSpec::cascade_without_build_stamp` warns about for `none`,
    // silently and with no warning, because the spec DID ask for a stamp.
    let spec = dual_libc_spec_with("date");
    let lock = ocx_python::parse_pylock(DUAL_LIBC_LOCK).unwrap();
    let build_ts = normalizer::build_timestamp(&spec.build_timestamp);
    let stamp = build_ts.clone().expect("`date` yields a stamp");

    let entries =
        build_env_plan_entries(&spec, &lock, "1.0.0", &[], &VersionPlatformMap::default(), &build_ts).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].version,
        format!("1.0.0_{stamp}"),
        "the published tag carries the build stamp, as on the archive path"
    );
    assert_eq!(
        entries[0].source_version, "1.0.0",
        "the source version stays bare — the stamp is a publish-time property"
    );
    // Stamping is orthogonal to wheel selection: both libc keys still resolve.
    assert_eq!(entries[0].platforms, vec!["linux/amd64".to_string()]);
    assert_eq!(entries[0].assets.len(), 2);
}

#[test]
fn build_env_plan_entries_keeps_the_bare_tag_without_a_build_timestamp() {
    let spec = dual_libc_spec_with("none");
    let lock = ocx_python::parse_pylock(DUAL_LIBC_LOCK).unwrap();
    let build_ts = normalizer::build_timestamp(&spec.build_timestamp);
    assert!(build_ts.is_none(), "`none` must yield no stamp");

    let entries =
        build_env_plan_entries(&spec, &lock, "1.0.0", &[], &VersionPlatformMap::default(), &build_ts).unwrap();

    assert_eq!(entries[0].version, "1.0.0");
    assert_eq!(entries[0].source_version, "1.0.0");
}

#[test]
fn build_env_plan_entries_keeps_a_stamp_off_a_version_ocx_cannot_parse() {
    // A >3-component PEP 440 release (`0.0.0.2`) is not an `ocx_lib::Version`,
    // so no build stamp can be appended to it. It must keep its bare tag
    // rather than be dropped the way the archive path drops an unnormalizable
    // version — PyPI publishes these routinely, and push already treats such a
    // version as non-cascadable through the same `Version::parse` gate.
    let lock = ocx_python::parse_pylock(
        r#"
lock-version = "1.0"

[[packages]]
name = "pycowsay"
version = "0.0.0.2"

[[packages.wheels]]
name = "pycowsay-0.0.0.2-py3-none-any.whl"
url = "https://example.com/pycowsay-0.0.0.2-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#,
    )
    .unwrap();
    let spec = dual_libc_spec_with("date");
    let build_ts = normalizer::build_timestamp(&spec.build_timestamp);

    let entries =
        build_env_plan_entries(&spec, &lock, "0.0.0.2", &[], &VersionPlatformMap::default(), &build_ts).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].version, "0.0.0.2");
    assert_eq!(entries[0].source_version, "0.0.0.2");
}

// ── wheels-key → selection-constraint derivation ─────────────────────────

#[test]
fn wheel_target_constraints_derives_libc_and_filter_per_key() {
    let wheels: WheelPatterns = serde_yaml_ng::from_str(concat!(
        "linux/amd64: ~\n",
        "\"linux/arm64+libc.glibc\": ~\n",
        "\"linux/arm64+libc.musl\": ~\n",
        "windows/amd64: ~\n",
    ))
    .unwrap();
    let by_string = |wanted: &str| {
        wheels
            .filters
            .keys()
            .find(|platform| platform.to_string() == wanted)
            .expect("key present")
    };

    // Plain linux key: default `["any"]` filter, gnu libc (no musllinux
    // prefix in the filter), always a NON-empty wheel_priority.
    let plain = wheel_target_constraints(&wheels, by_string("linux/amd64"));
    assert_eq!(plain.libc, Some(LibcFamily::Gnu));
    assert_eq!(plain.wheel_priority, Some(vec!["any".to_string()]));
    assert_eq!(plain.min_manylinux, None, "floors stay select-defaulted");
    assert_eq!(plain.abi, None, "python.abi remains the one ABI pin");

    let glibc = wheel_target_constraints(&wheels, by_string("linux/arm64+libc.glibc"));
    assert_eq!(glibc.libc, Some(LibcFamily::Gnu));
    assert_eq!(
        glibc.wheel_priority,
        Some(vec!["manylinux".to_string(), "any".to_string()])
    );

    let musl = wheel_target_constraints(&wheels, by_string("linux/arm64+libc.musl"));
    assert_eq!(musl.libc, Some(LibcFamily::Musl));
    assert_eq!(
        musl.wheel_priority,
        Some(vec!["musllinux".to_string(), "any".to_string()])
    );

    let windows = wheel_target_constraints(&wheels, by_string("windows/amd64"));
    assert_eq!(
        windows.libc,
        Some(LibcFamily::Gnu),
        "libc is a linux axis; gnu is inert elsewhere"
    );
    assert_eq!(windows.wheel_priority, Some(vec!["win".to_string(), "any".to_string()]));
}

#[test]
fn wheel_target_constraints_plain_key_with_musllinux_filter_selects_musl_tag_set() {
    // A plain key whose EXPLICIT filter admits only musllinux wheels
    // selects against the musl uv tag set (gnu would exclude them all).
    let wheels: WheelPatterns = serde_yaml_ng::from_str("linux/amd64: [musllinux, any]\n").unwrap();
    let platform = wheels.filters.keys().next().unwrap();

    let constraints = wheel_target_constraints(&wheels, platform);
    assert_eq!(constraints.libc, Some(LibcFamily::Musl));
    assert_eq!(
        constraints.wheel_priority,
        Some(vec!["musllinux".to_string(), "any".to_string()])
    );
}
