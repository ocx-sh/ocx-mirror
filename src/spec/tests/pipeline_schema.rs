// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── §3.1 S1: Pipeline schema round-trip and validation tests ────────────

#[test]
fn round_trip_full_pipeline_spec() {
    // §3.1: Round-trip: valid mirror.yml with full tests:, platforms:,
    // ocx_mirror:, notify: blocks parses correctly.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
  - name: smoke
    command: bash ./tests/smoke.sh

platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
      - image: alpine:3.20
        shell: sh
  darwin/arm64:
    runner: macos-latest
    shell: bash
  windows/amd64:
    runner: windows-latest
    shell: pwsh
    tests:
      - name: version
        command: shfmt.exe --version

ocx_mirror:
  rev: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();

    // tests block
    let tests = spec.tests.as_ref().unwrap();
    assert_eq!(tests.len(), 2);
    assert_eq!(tests[0].name, "version");
    assert_eq!(tests[0].command.as_deref(), Some("shfmt --version"));
    assert_eq!(tests[1].name, "smoke");

    // platforms block
    let platforms = spec.platforms.as_ref().unwrap();
    assert!(platforms.contains_key("linux/amd64"));
    assert!(platforms.contains_key("darwin/arm64"));
    assert!(platforms.contains_key("windows/amd64"));

    let linux = &platforms["linux/amd64"];
    assert_eq!(linux.runner, "ubuntu-latest");
    let containers = linux.containers.as_ref().unwrap();
    assert_eq!(containers.len(), 2);
    assert_eq!(containers[0].image, "ubuntu:24.04");
    assert_eq!(containers[0].shell.as_deref(), Some("bash"));
    assert_eq!(containers[1].image, "alpine:3.20");

    // per-platform test override
    let windows = &platforms["windows/amd64"];
    let win_tests = windows.tests.as_ref().unwrap();
    assert_eq!(win_tests.len(), 1);
    assert_eq!(win_tests[0].name, "version");
    assert_eq!(win_tests[0].command.as_deref(), Some("shfmt.exe --version"));

    // ocx_mirror block
    let ocx_mirror = spec.ocx_mirror.as_ref().unwrap();
    assert_eq!(
        ocx_mirror.rev.as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );

    // notify block
    let notify = spec.notify.as_ref().unwrap();
    let discord = notify.discord.as_ref().unwrap();
    assert_eq!(discord.webhook_secret, "DISCORD_WEBHOOK_URL");
}

#[test]
fn validate_empty_tests_array() {
    // §3.1: Rejection — empty tests: array
    let yaml = format!(
        r#"{base}
tests: []
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("tests") && (e.contains("empty") || e.contains("least"))),
        "Expected error about empty tests:, got: {errors:?}"
    );
}

#[test]
fn validate_duplicate_test_names() {
    // §3.1: Rejection — duplicate tests[].name
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
  - name: version
    command: shfmt --version --again
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("duplicate") || e.contains("unique")),
        "Expected duplicate test name error, got: {errors:?}"
    );
}

#[test]
fn validate_invalid_test_name_starts_with_digit() {
    // §3.1: Rejection — invalid tests[].name (starts with digit)
    let yaml = format!(
        r#"{base}
tests:
  - name: 1version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("name") || e.contains("invalid")),
        "Expected invalid test name error, got: {errors:?}"
    );
}

#[test]
fn validate_invalid_platform_key_no_arch() {
    // §3.1: Rejection — invalid platform key (linux without arch)
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux:
    runner: ubuntu-latest
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("platform") || e.contains("linux")),
        "Expected invalid platform key error, got: {errors:?}"
    );
}

#[test]
fn validate_platform_missing_runner() {
    // §3.1: Rejection — missing runner on declared platform
    // PlatformConfig.runner is required (non-optional) so this fails at
    // parse time with serde error.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
        base = MINIMAL_BASE_YAML
    );

    // Missing required `runner` field → serde parse error
    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(&yaml);
    assert!(result.is_err(), "Expected parse error for missing runner, but got Ok");
}

#[test]
fn validate_empty_containers_array() {
    // §3.1: Rejection — empty containers: array (must be absent OR ≥1)
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers: []
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("container") && (e.contains("empty") || e.contains("least"))),
        "Expected error about empty containers:, got: {errors:?}"
    );
}

#[test]
fn validate_ambiguous_shell_on_nonstandard_image() {
    // §3.1: Rejection — ambiguous shell on non-standard image (no default)
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: mycorp/custom-runner:1.0
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("shell") || e.contains("ambiguous")),
        "Expected ambiguous shell error for non-standard image, got: {errors:?}"
    );
}

#[test]
fn validate_platform_rejects_variant_prefixed_min_version() {
    // Applicability keys off the release core; a variant-prefixed bound would
    // compare asymmetrically against the stripped version and silently misfilter.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    min_version: "debug-0.11.7"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("min_version") && e.contains("plain version")),
        "variant-prefixed min_version must be rejected, got: {errors:?}"
    );
}

#[test]
fn validate_platform_rejects_build_stamped_max_version() {
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    max_version: "1.0.0_20260101"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("max_version") && e.contains("plain version")),
        "build-stamped max_version must be rejected, got: {errors:?}"
    );
}

#[test]
fn validate_platform_rejects_inverted_window() {
    // min ≥ max silently drops the platform from every version — must error.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    min_version: "5.0.0"
    max_version: "2.0.0"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("min_version") && e.contains("must be below")),
        "inverted min/max window must be rejected, got: {errors:?}"
    );
}

#[test]
fn validate_rejects_containers_on_a_non_linux_platform() {
    // Container legs are `docker run` on a Linux runner. A macOS or Windows
    // runner has no Linux container engine, so the pairing can only fail at
    // run time — after a full matrix spin-up. Reject it at generate time.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  windows/amd64:
    runner: windows-latest
    containers:
      - image: ubuntu:24.04
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("containers are linux-only")),
        "containers on a windows platform must be rejected, got: {errors:?}"
    );
}

/// A minimal spec whose single container carries the given extra lines,
/// indented to sit under `- image: alpine:3.20`.
fn spec_with_container_lines(lines: &str) -> MirrorSpec {
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
{lines}"#,
        base = MINIMAL_BASE_YAML
    );
    serde_yaml_ng::from_str(&yaml).unwrap()
}

#[test]
fn validate_accepts_setup_commands_on_a_container() {
    let spec = spec_with_container_lines(
        r#"        setup:
          - apk add --no-cache libstdc++
          - apk add --no-cache libgcc
"#,
    );
    let errors = spec.validate(Path::new("test.yml"));
    assert!(errors.is_empty(), "setup commands must validate, got: {errors:?}");
}

#[test]
fn validate_rejects_an_empty_setup_list() {
    // `setup: []` reads as "provision nothing" but declares intent to
    // provision — the maintainer meant to fill it in.
    let spec = spec_with_container_lines("        setup: []\n");
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("empty setup list")),
        "an empty setup list must be rejected, got: {errors:?}"
    );
}

#[test]
fn validate_rejects_a_blank_setup_command() {
    let spec = spec_with_container_lines(
        r#"        setup:
          - "  "
"#,
    );
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("setup[0] must not be blank")),
        "a blank setup command must be rejected, got: {errors:?}"
    );
}

#[test]
fn validate_rejects_a_multi_line_setup_command() {
    // One entry is one `RUN`. A block scalar here — the shape `script_inline`
    // trains maintainers to reach for — would emit a broken Dockerfile.
    let spec = spec_with_container_lines(
        r#"        setup:
          - |
            apk add --no-cache libstdc++
            apk add --no-cache libgcc
"#,
    );
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.iter().any(|e| e.contains("setup[0] must be a single command")),
        "a multi-line setup command must be rejected, got: {errors:?}"
    );
}

#[test]
fn validate_rejects_a_trailing_backslash_setup_command() {
    // A line continuation absorbs the next `RUN` as its own arguments: the
    // build exits 0 with that layer never applied, and the leg goes green
    // on an image the setup did not provision.
    // Both spellings: whitespace after the backslash does not stop docker
    // continuing the line, so it must not stop the check either.
    for trailer in ["", " "] {
        let spec = spec_with_container_lines(&format!(
            r#"        setup:
          - "apk add --no-cache libstdc++ \\{trailer}"
          - apk add --no-cache libgcc
"#
        ));
        let errors = spec.validate(Path::new("test.yml"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("setup[0] must not end with a backslash")),
            "a trailing-backslash setup command must be rejected (trailer {trailer:?}), got: {errors:?}"
        );
    }
}

#[test]
fn validate_rejects_setup_on_a_platform_without_containers() {
    // `setup:` belongs to a container, not a platform. One level of
    // under-indentation is the whole mistake, and `deny_unknown_fields` is
    // what turns it into a parse error instead of a dropped line — so this
    // never reaches `validate()`.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    setup:
      - apk add --no-cache libstdc++
"#,
        base = MINIMAL_BASE_YAML
    );
    let error = serde_yaml_ng::from_str::<MirrorSpec>(&yaml)
        .expect_err("a platform-level setup must fail to parse")
        .to_string();
    assert!(
        error.contains("unknown field") && error.contains("setup"),
        "the error must name the rejected key, got: {error}"
    );
}

#[test]
fn validate_accepts_a_libc_bearing_platform_key() {
    // Declaring a libc is the only way to make the claim testable, so the
    // key grammar has to admit it — a `^[a-z0-9_-]+/[a-z0-9_-]+$` regex does
    // not, and that alone kept every libc mirror out of the test matrix.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  "linux/amd64+libc.musl":
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors.is_empty(),
        "a musl claim tested in an alpine image must validate, got: {errors:?}"
    );
}

#[test]
fn validate_rejects_a_container_whose_libc_contradicts_the_platform_key() {
    // The silent direction: a musl-static artifact runs fine under glibc, so
    // testing a `+libc.musl` claim inside ubuntu goes GREEN having verified
    // nothing. The mismatch must be named, not rendered.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  "linux/amd64+libc.musl":
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("ubuntu:24.04") && e.contains("libc.glibc") && e.contains("libc.musl")),
        "the error must name the image and both libcs, got: {errors:?}"
    );
}

#[test]
fn validate_rejects_the_loud_libc_mismatch_too() {
    // The other direction fails at run time rather than passing falsely, but
    // it is the same authoring mistake — reject it symmetrically instead of
    // spending a matrix run to learn it.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  "linux/amd64+libc.glibc":
    runner: ubuntu-latest
    containers:
      - image: alpine:3.20
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("alpine:3.20") && e.contains("libc.musl")),
        "an alpine image under a glibc claim must be rejected, got: {errors:?}"
    );
}

#[test]
fn platform_slug_separates_libc_variants_and_leaves_plain_keys_alone() {
    // The join key between `pipeline prepare` (work-dir basename), the CI
    // renderer (bundle + JUnit filenames) and `pipeline push` (lookup).
    assert_eq!(platform_key_slug("linux/amd64+libc.musl"), "linux_amd64_libc.musl");
    assert_eq!(platform_key_slug("linux/amd64+libc.glibc"), "linux_amd64_libc.glibc");
    // Plain keys keep exactly the slug they had — this is what the pinned
    // mirror corpus renders with.
    for key in ["linux/amd64", "linux/arm64", "darwin/arm64", "windows/amd64"] {
        assert_eq!(platform_key_slug(key), key.replace('/', "_"));
    }

    // Docker never sees the suffix; everything else keeps it.
    assert_eq!(platform_without_features("linux/amd64+libc.musl"), "linux/amd64");
    assert_eq!(platform_without_features("linux/arm64"), "linux/arm64");
}

#[test]
fn image_inference_keys_off_the_repository_basename() {
    // A registry-qualified reference must classify like its bare form —
    // otherwise a mirror that spells out `docker.io/library/alpine` gets a
    // gnu ocx that cannot start under musl, and a `bash` that is not there.
    assert_eq!(infer_libc_from_image("alpine:3.20"), "musl");
    assert_eq!(infer_libc_from_image("docker.io/library/alpine:3.20"), "musl");
    assert_eq!(infer_libc_from_image("ubuntu:24.04"), "gnu");
    assert_eq!(infer_libc_from_image("fedora:40"), "gnu");
    assert_eq!(infer_shell_from_image("docker.io/library/alpine:3.20"), Some("sh"));
    assert_eq!(infer_shell_from_image("ghcr.io/acme/fedora:40"), Some("bash"));

    // The join key with `pipeline push`: dots slugify too.
    assert_eq!(image_to_container_id("ubuntu:24.04"), "ubuntu_24_04");
    assert_eq!(image_to_container_id("alpine:3.20"), "alpine_3_20");
    assert_eq!(image_to_container_id("ghcr.io/acme/img:1.0"), "ghcr_io_acme_img_1_0");
}

#[test]
fn validate_exclude_rejects_inverted_range_and_variant_version() {
    // exclude[0]: inverted range matches nothing (silent no-op).
    // exclude[1]: variant-prefixed single version compares asymmetrically.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    exclude:
      - min_version: "9.4.0"
        max_version: "5.0.0"
      - version: "debug-1.0.0"
"#,
        base = MINIMAL_BASE_YAML
    );
    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("exclude[0]") && e.contains("must be below")),
        "inverted exclude range must be rejected, got: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("exclude[1]") && e.contains("plain version")),
        "variant-prefixed exclude version must be rejected, got: {errors:?}"
    );
}

#[test]
fn containers_need_no_ocx_mirror_block() {
    // Declaring `containers:` once demanded an `ocx_mirror.release_tag`
    // that nothing rendered — a required field with no consumer. The ocx
    // the legs download is the renderer's own `OCX_CONTAINER_CLI_TAG`, so
    // the spec has nothing to say about it. A container spec with no
    // `ocx_mirror:` block at all must validate clean.
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: ubuntu:24.04
        shell: bash
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    assert!(spec.ocx_mirror.is_none(), "fixture must carry no ocx_mirror block");
    let errors = spec.validate(Path::new("test.yml"));
    assert!(errors.is_empty(), "expected a clean spec, got: {errors:?}");
}

#[test]
fn validate_rev_not_40_hex() {
    // §3.1: Rejection — rev not 40-hex
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
ocx_mirror:
  rev: "short"
"#,
        base = MINIMAL_BASE_YAML
    );

    let spec: MirrorSpec = serde_yaml_ng::from_str(&yaml).unwrap();
    let errors = spec.validate(Path::new("test.yml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("rev") || e.contains("hex") || e.contains("40")),
        "Expected invalid rev format error, got: {errors:?}"
    );
}

#[test]
fn validate_rejects_ocx_install_block() {
    // §3.1: Rejection — ocx_install: block present at all → SpecUsageError
    // Catches early adopters who copied an earlier draft spec.
    // Since ocx_install is not in the schema, serde rejects unknown fields
    // OR it silently ignores them (depends on #[serde(deny_unknown_fields)]).
    // We test via validate() returning an error for this field.
    //
    // Implementation note: the validator should check for `ocx_install` key
    // via a raw YAML pass or a dedicated sentinel field, and emit:
    //   "ocx binary is installed via direct release download; remove `ocx_install:` block"
    let yaml = format!(
        r#"{base}
tests:
  - name: version
    command: shfmt --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
ocx_install: {{}}
"#,
        base = MINIMAL_BASE_YAML
    );

    // If serde rejects unknown fields, this is a parse error.
    // If serde ignores unknown fields, it's a validate() error.
    // Either satisfies the rejection requirement.
    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(&yaml);
    match result {
        Err(_) => {
            // serde rejected the unknown field — test passes
        }
        Ok(spec) => {
            let errors = spec.validate(Path::new("test.yml"));
            assert!(
                errors
                    .iter()
                    .any(|e| e.contains("ocx_install") || e.contains("release download")),
                "Expected rejection of ocx_install: block, got: {errors:?}"
            );
        }
    }
}
