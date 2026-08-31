// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The test matrix: one leg per `(platform, container)` pair, and the shell
//! that runs the declared tests inside it.
//!
//! The largest single concern in the renderer — a leg carries the runner, the
//! image, the shell, the libc, and the `container_id` the JUnit filename is
//! keyed by, and `pipeline push` looks results back up by exactly that key.

use super::WORKFLOW_TEMPLATE;
use crate::spec::{self, MirrorSpec, PlatformConfig, TestEntry};

/// The kind of a rendered test entry — mirrors [`spec::TestKind`] but owns its
/// payload so it can outlive the spec borrow in `MatrixLeg`.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderedTestKind {
    Command(String),
    Script(String),
    ScriptInline(String),
}

/// One rendered test entry carried in a matrix leg.
#[derive(Debug, Clone)]
pub struct RenderedTest {
    name: String,
    kind: RenderedTestKind,
}

/// Describes one matrix leg (test job matrix entry).
///
/// A native leg has an empty `container_image` and the sentinel `container_id`
/// `_native_`. A container leg carries the image, its libc family (which ocx
/// release binary to mount) and a stable `container_id` taken from the config
/// `id` or the slugified image. Downstream consumers (`pipeline push`,
/// `junit.rs`) key on `(version, platform, container)` triples in JUnit XML and
/// run-summary.json, so `container_id` stays meaningful in both modes.
pub struct MatrixLeg {
    platform: String,
    platform_slug: String,
    runner: String,
    container_id: String,
    /// Container image reference; empty for a native leg.
    container_image: String,
    /// Container libc family (`musl` / `gnu`); empty for a native leg.
    container_libc: String,
    /// `platform` with any `os.features` suffix stripped — what
    /// `docker run --platform` accepts. Empty for a native leg.
    docker_platform: String,
    /// Dockerfile provisioning this leg's image from the container's `setup:`
    /// commands. Empty when the container declares none — which is also what
    /// gates every piece of setup machinery out of the rendered workflow.
    container_dockerfile: String,
    shell: String,
    tests: Vec<RenderedTest>,
}

/// Render the Dockerfile that provisions one container leg.
///
/// `RUN` in shell form hands the line to the image's `SHELL` unparsed, so the
/// YAML → Rust → Dockerfile → shell trip copies the command through. The shapes
/// that would not arrive as one `RUN` — an embedded newline, a trailing
/// backslash — are rejected by `validate_container_setup` before the renderer
/// sees them. A `${{ … }}` in a command is emitted raw and interpolated by
/// Actions, the same surface `tests[].command` already has.
pub fn render_setup_dockerfile(image: &str, shell: &str, setup: &[String]) -> String {
    // `{shell:?}` is the JSON-quoted exec form `SHELL` requires.
    let mut dockerfile = format!("FROM {image}\nSHELL [{shell:?}, \"-c\"]\n");
    for command in setup {
        dockerfile.push_str(&format!("RUN {command}\n"));
    }
    dockerfile
}

/// Does any leg provision its image?
///
/// One gate for the whole feature: the matrix key, the `env:` passthrough and
/// the build block are meaningless apart, so they appear together or not at
/// all — and a spec declaring no `setup:` renders exactly what it did before.
pub fn any_container_setup(legs: &[MatrixLeg]) -> bool {
    legs.iter().any(|leg| !leg.container_dockerfile.is_empty())
}

/// Convert a slice of [`TestEntry`] into [`RenderedTest`] list.
///
/// Entries that fail `kind()` (i.e. validated-invalid specs that slip through)
/// are silently omitted — `validate_tests` is the authoritative gate.
pub fn render_tests(entries: &[TestEntry]) -> Vec<RenderedTest> {
    entries
        .iter()
        .filter_map(|t| {
            let kind = match t.kind() {
                Ok(spec::TestKind::Command(cmd)) => RenderedTestKind::Command(cmd.to_owned()),
                Ok(spec::TestKind::Script(p)) => RenderedTestKind::Script(p.display().to_string()),
                Ok(spec::TestKind::ScriptInline(src)) => RenderedTestKind::ScriptInline(src.to_owned()),
                Err(_) => return None,
            };
            Some(RenderedTest {
                name: t.name.clone(),
                kind,
            })
        })
        .collect()
}

/// Build the flat list of matrix legs from a `MirrorSpec`.
pub fn build_matrix(spec: &MirrorSpec) -> Vec<MatrixLeg> {
    let Some(platforms) = &spec.platforms else {
        return Vec::new();
    };

    let top_level_tests: Vec<RenderedTest> = render_tests(spec.tests.as_deref().unwrap_or(&[]));

    // Stable ordering: sort platform keys alphabetically so the generated YAML
    // is deterministic across runs.
    let mut platform_keys: Vec<&String> = platforms.keys().collect();
    platform_keys.sort();

    let mut legs = Vec::new();
    for platform_key in platform_keys {
        let config = &platforms[platform_key];
        // Must equal the basename `pipeline prepare` gives this platform's work
        // directory — the workflow flattens that into `bundle-{V}-{slug}.tar.xz`
        // and names the leg's JUnit file with it. A libc-bearing key slugs to
        // `linux_amd64_libc.musl` there, so `replace('/', "_")` here pointed the
        // leg at a bundle that does not exist.
        let platform_slug = spec::platform_key_slug(platform_key);

        let effective_tests: Vec<RenderedTest> = config
            .tests
            .as_deref()
            .map(render_tests)
            .unwrap_or_else(|| top_level_tests.clone());

        match config.containers.as_deref().filter(|c| !c.is_empty()) {
            // Container mode: one leg per image, each testing the same artifact
            // under a different userland.
            Some(containers) => {
                for container in containers {
                    // Same slug `pipeline push` uses to find this leg's JUnit
                    // file. Diverging here loses every container result.
                    let container_id = container
                        .id
                        .clone()
                        .unwrap_or_else(|| spec::image_to_container_id(&container.image));
                    // Validation guarantees an explicit shell whenever the image
                    // has no known default, so the fallback is unreachable for a
                    // validated spec; POSIX `sh` is the safest thing to guess.
                    let shell = container.shell.clone().unwrap_or_else(|| {
                        spec::infer_shell_from_image(&container.image)
                            .unwrap_or("sh")
                            .to_string()
                    });
                    // Validation rejects an empty `setup:`, so the filter only
                    // guards against a spec that skipped it — an empty list
                    // must render as "no setup", never as a bare `FROM`.
                    let container_dockerfile = container
                        .setup
                        .as_deref()
                        .filter(|setup| !setup.is_empty())
                        .map(|setup| render_setup_dockerfile(&container.image, &shell, setup))
                        .unwrap_or_default();
                    legs.push(MatrixLeg {
                        platform: platform_key.clone(),
                        platform_slug: platform_slug.clone(),
                        runner: config.runner.clone(),
                        container_id,
                        container_image: container.image.clone(),
                        container_libc: spec::infer_libc_from_image(&container.image).to_string(),
                        docker_platform: spec::platform_without_features(platform_key),
                        container_dockerfile,
                        shell,
                        tests: effective_tests.clone(),
                    });
                }
            }
            None => {
                let shell = native_shell_for_platform(platform_key, config);
                legs.push(MatrixLeg {
                    platform: platform_key.clone(),
                    platform_slug: platform_slug.clone(),
                    runner: config.runner.clone(),
                    container_id: "_native_".to_string(),
                    container_image: String::new(),
                    container_libc: String::new(),
                    docker_platform: String::new(),
                    container_dockerfile: String::new(),
                    shell: shell.to_string(),
                    tests: effective_tests,
                });
            }
        }
    }
    legs
}

/// Determine the shell for a native test leg.
pub fn native_shell_for_platform<'a>(platform: &str, config: &'a PlatformConfig) -> &'a str {
    if let Some(shell) = &config.shell {
        return shell.as_str();
    }
    if platform.starts_with("windows") {
        "pwsh"
    } else {
        "bash"
    }
}

/// Render the YAML matrix `include:` entries for the test job.
///
/// Test commands are inlined as a YAML list so the workflow references them
/// via `${{ matrix.tests }}`. This ensures per-platform test overrides
/// (e.g. `cmake.exe --version` on `windows/amd64`) appear verbatim in the
/// generated YAML, satisfying golden-test assertions.
pub fn render_matrix_entries(legs: &[MatrixLeg]) -> String {
    let mut out = String::new();
    for leg in legs {
        out.push_str(&format!(
            "          - platform: {}\n            platform_slug: {}\n            runner: {}\n            container_id: {}\n",
            leg.platform, leg.platform_slug, leg.runner, leg.container_id,
        ));
        // Emitted only for container legs, so a native-only workflow keeps the
        // exact key set it had before container mode existed (zero drift for the
        // pinned mirror corpus). The test step reads an absent/empty
        // `container_image` as native mode.
        if !leg.container_image.is_empty() {
            out.push_str(&format!(
                "            container_image: {:?}\n            container_libc: {:?}\n            docker_platform: {:?}\n",
                leg.container_image, leg.container_libc, leg.docker_platform,
            ));
        }
        // Second guarded block, for the same reason: a container leg that
        // declares no `setup:` keeps the exact key set it had before the field
        // existed, so provisioning is opt-in per container, not per leg set.
        if !leg.container_dockerfile.is_empty() {
            // Block scalar `|` — the Dockerfile is multi-line and carries
            // whatever quoting the setup commands do. Body at 14 spaces
            // (matrix entry indent 12 + 2), same shape as `script_inline`.
            let indented = leg
                .container_dockerfile
                .lines()
                .map(|line| format!("              {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&format!("            container_dockerfile: |\n{indented}\n"));
        }
        out.push_str(&format!("            shell: {}\n", leg.shell));
        // Inline the test entries so they are visible in the generated YAML.
        out.push_str("            tests:\n");
        for test in &leg.tests {
            match &test.kind {
                RenderedTestKind::Command(cmd) => {
                    out.push_str(&format!(
                        "              - name: {}\n                kind: command\n                command: {}\n",
                        test.name, cmd
                    ));
                }
                RenderedTestKind::Script(path) => {
                    out.push_str(&format!(
                        "              - name: {}\n                kind: script\n                script: {}\n",
                        test.name, path
                    ));
                }
                RenderedTestKind::ScriptInline(src) => {
                    // Use YAML block scalar `|` so multi-line Starlark survives.
                    // Each line of the inline source is indented 18 spaces
                    // (matrix entry indent 14 + 4 for block scalar body).
                    let indented = src
                        .lines()
                        .map(|line| format!("                  {line}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push_str(&format!(
                        "              - name: {}\n                kind: script_inline\n                script_inline: |\n{indented}\n",
                        test.name
                    ));
                }
            }
        }
    }
    out
}

/// The one `ocx` CLI release a generated workflow runs, on every leg.
///
/// Both legs read it: native legs pass it to `setup-ocx` as its `version:`
/// input, container legs download the statically-linked release of the same tag
/// and mount it into the image. Left unpinned, the native legs would float to
/// whatever `ocx` is newest on the day a mirror happens to run while the
/// container legs stayed on this constant — the two halves of one test matrix
/// exercising different binaries.
///
/// Deliberately a renderer constant and not a spec field. Which `ocx` executes
/// the tests is a property of *this renderer* — it has to be able to read the
/// bundles this version writes — so it moves when the renderer moves, via each
/// repository's `ocx.lock`. A per-repo knob would let forty mirrors drift onto
/// forty different test binaries, which is the failure this pin exists to
/// prevent.
///
/// The trailing marker is the Renovate anchor; see `customManagers` in
/// `renovate.json`. Keep the literal on one line or the regex stops matching.
pub const OCX_CONTAINER_CLI_TAG: &str = "v0.6.0"; // renovate: datasource=github-releases depName=ocx-sh/ocx

/// [`OCX_CONTAINER_CLI_TAG`] as `setup-ocx` spells it: a bare semver, no `v`.
///
/// Derived rather than a second constant so the two spellings cannot drift —
/// Renovate only ever moves the tag.
pub fn ocx_cli_version() -> &'static str {
    OCX_CONTAINER_CLI_TAG.trim_start_matches('v')
}

/// Render per-test shell commands for the `test` job's run step.
///
/// Each matrix leg runs all its tests for every discovered version. The renderer
/// emits a single shell block that iterates per-version.
///
/// A native leg (empty `container_image`) calls `ocx package test` directly on
/// the runner. A container leg fetches a libc-matched, statically-linked `ocx`
/// release and wraps every `ocx package test` in `docker run <image>` — so the
/// mirrored artifact is actually executed by that image's loader against that
/// image's libc, which is the only way an `os.features` musl/glibc claim can be
/// verified. JS actions keep running on the host's glibc node throughout, which
/// is why this is a per-command wrapper rather than a job-level `container:`.
///
/// `is_env` switches what is handed to `ocx package test`: one bundle path for
/// an archive/binary source, `-m <metadata> <layers…>` for an env source, whose
/// artifact is a composed metadata document plus N wheel layers. Both forms are
/// resolved into shell variables by [`test_target_resolve_script`].
pub fn render_test_run_steps(legs: &[MatrixLeg], is_env: bool) -> String {
    if legs.is_empty() {
        return String::new();
    }

    // What `ocx package test` is pointed at, and the `--platform` it declares.
    // An env container leg names its own libc as an os_feature: `ocx package
    // test` threads `--platform` verbatim into DEPENDENCY resolution, and an
    // env's interpreter index may carry per-libc entries only — a bare
    // `linux/amd64` request would match none of them. Archive legs keep the
    // bare matrix platform, so their output is unchanged.
    let (test_target, test_platform) = if is_env {
        (r#"-m "${METADATA}" ${LAYERS}"#, r#""${TEST_PLATFORM}""#)
    } else {
        (r#""${BUNDLE}""#, r#""${{ matrix.platform }}""#)
    };

    // `set -u` is in force, so this line may only be emitted where `BUNDLE`
    // exists — an env leg never sets it.
    let metadata_sibling = if is_env {
        ""
    } else {
        "            METADATA_SIBLING=\"${BUNDLE%.tar.xz}-metadata.json\"\n"
    };

    // Emitted only when some leg declares an image, so native-only workflows
    // stay byte-identical to the pre-container renderer. `{OCX_TEST}` is the
    // command prefix: the `ocx_test` wrapper under container mode, plain `ocx`
    // otherwise.
    let has_container = legs.iter().any(|leg| !leg.container_image.is_empty());
    let (container_prelude, ocx_test) = if has_container {
        (
            r#"            # Container legs: fetch a libc-matched static ocx once, then run every
            # `ocx package test` inside `docker run <image>` so the mirrored
            # artifact is executed by that image's loader. Native legs (empty
            # container_image, e.g. the macOS/Windows legs of a mixed spec) call
            # the runner's own ocx unchanged.
            CONTAINER_IMAGE="${{ matrix.container_image }}"
            if [ -n "${CONTAINER_IMAGE}" ]; then
              # `docker_platform` is `platform` minus any `os.features` suffix:
              # docker rejects `linux/amd64+libc.musl` outright, while the matrix
              # label, the `ocx package test --platform` flag and the discover
              # platform set all need the full, unambiguous key.
              case "${{ matrix.docker_platform }}" in
                linux/amd64) OCX_ARCH=x86_64 ;;
                linux/arm64) OCX_ARCH=aarch64 ;;
                *) echo "::error::no static ocx release for container platform ${{ matrix.docker_platform }} (linux/amd64 and linux/arm64 only)"; exit 1 ;;
              esac
              # Container legs run the artifact natively — no qemu is installed,
              # so a leg whose runner is a different architecture cannot execute
              # the image at all. Say that, rather than let docker fail with a
              # bare exec-format error several minutes in.
              RUNNER_ARCH_UNAME="$(uname -m)"
              if [ "${RUNNER_ARCH_UNAME}" != "${OCX_ARCH}" ]; then
                echo "::error::container legs for ${{ matrix.docker_platform }} need a ${OCX_ARCH} runner (this one is ${RUNNER_ARCH_UNAME}); set an arch-matched \`runner:\` on this platform"
                exit 1
              fi
              OCX_TRIPLE="${OCX_ARCH}-unknown-linux-${{ matrix.container_libc }}"
              OCX_CONTAINER_DIR="${RUNNER_TEMP}/ocx-${OCX_TRIPLE}"
              OCX_CONTAINER_BIN="${OCX_CONTAINER_DIR}/ocx"
              if [ ! -x "${OCX_CONTAINER_BIN}" ]; then
                mkdir -p "${OCX_CONTAINER_DIR}"
                # cargo-dist archives carry a single top-level directory, so
                # --strip-components=1 lands the binary directly. A layout change
                # fails the chmod instead of silently testing nothing.
                curl -fsSL "https://github.com/ocx-sh/ocx/releases/download/{OCX_CLI_TAG}/ocx-${OCX_TRIPLE}.tar.gz" \
                  | tar -xz -C "${OCX_CONTAINER_DIR}" --strip-components=1
                chmod +x "${OCX_CONTAINER_BIN}"
              fi
              # Pull the image here rather than letting `docker run` do it
              # implicitly: a rate-limited or flaky pull inside the test loop is
              # recorded as a failed testcase, indistinguishable from a mirrored
              # artifact that genuinely does not run. Failing this step instead
              # keeps a red testcase meaning exactly one thing. Guarded by
              # `inspect` — that is when `docker run` would have pulled anyway,
              # and a pull per version would spend a manifest request (the thing
              # being rate-limited) on every one. It also precedes the setup
              # build below, so a provisioned image's FROM resolves from cache.
              if ! docker image inspect "${CONTAINER_IMAGE}" >/dev/null 2>&1; then
                OCX_PULL_ATTEMPT=1
                OCX_PULL_DELAY=2
                until docker pull --platform "${{ matrix.docker_platform }}" "${CONTAINER_IMAGE}"; do
                  if [ "${OCX_PULL_ATTEMPT}" -ge 5 ]; then
                    echo "::error::could not pull ${CONTAINER_IMAGE} for ${{ matrix.docker_platform }} after ${OCX_PULL_ATTEMPT} attempts (rate limit, network, or a tag with no ${{ matrix.docker_platform }} variant — see the docker output above)"
                    exit 1
                  fi
                  echo "pull of ${CONTAINER_IMAGE} failed (attempt ${OCX_PULL_ATTEMPT}); retrying in ${OCX_PULL_DELAY}s"
                  sleep "${OCX_PULL_DELAY}"
                  OCX_PULL_ATTEMPT=$((OCX_PULL_ATTEMPT + 1))
                  OCX_PULL_DELAY=$((OCX_PULL_DELAY * 2))
                done
              fi
{CONTAINER_SETUP_BUILD}            fi
            ocx_test() {
              if [ -n "${CONTAINER_IMAGE}" ]; then
                # The workspace is mounted at its own path so the bundle and its
                # `-metadata.json` sibling resolve identically inside and out.
                # OCX_HOME points into the container's own /tmp, so the install
                # the test runs against never touches the runner's filesystem.
                # The gnu ocx verifies TLS against the system CA store, which a
                # minimal base image need not carry; the musl build has webpki
                # roots baked in and ignores the mount.
                docker run --rm -i --platform "${{ matrix.docker_platform }}" \
                  -v "${GITHUB_WORKSPACE}:${GITHUB_WORKSPACE}" -w "${GITHUB_WORKSPACE}" \
                  -v "${OCX_CONTAINER_BIN}:/usr/local/bin/ocx:ro" \
                  -v /etc/ssl/certs/ca-certificates.crt:/etc/ssl/certs/ca-certificates.crt:ro \
                  -e OCX_HOME=/tmp/ocx-home -e OCX_NO_UPDATE_CHECK=1 \
                  "${CONTAINER_IMAGE}" ocx "$@"
              else
                ocx "$@"
              fi
            }
"#,
            "ocx_test",
        )
    } else {
        ("", "ocx")
    };

    // Provisioning sits inside the container branch, after the arch guard, so
    // an arm64-on-amd64 leg still gets its own diagnosis rather than a docker
    // exec-format error. The `inspect` guard makes the build once-per-leg: the
    // second and every later version of the run finds the tag already there.
    //
    // ponytail: the tag is runner-local and `(container_id, platform_slug)` is
    // unique per leg, which is enough even on a shared self-hosted runner. A
    // pathological `id:` can push it past docker's 128-char tag cap — that
    // fails the build loudly, upgrade to a hash if a real spec ever hits it.
    let container_setup_build = if any_container_setup(legs) {
        r#"              if [ -n "${OCX_CONTAINER_DOCKERFILE:-}" ]; then
                OCX_SETUP_TAG="ocx-mirror-setup:${{ matrix.container_id }}-${{ matrix.platform_slug }}"
                if ! docker image inspect "${OCX_SETUP_TAG}" >/dev/null 2>&1; then
                  printf '%s' "${OCX_CONTAINER_DOCKERFILE}" \
                    | docker build --platform "${{ matrix.docker_platform }}" -t "${OCX_SETUP_TAG}" - \
                    || { echo "::error::container setup failed for ${{ matrix.container_id }} on ${{ matrix.platform }}: a setup: command exited non-zero (the failing RUN is above)"; exit 1; }
                fi
                CONTAINER_IMAGE="${OCX_SETUP_TAG}"
              fi
"#
    } else {
        ""
    };

    let body = r#"{CONTAINER_PRELUDE}{METADATA_SIBLING}            mkdir -p junit
            JUNIT_FILE="junit/junit-${VERSION}-${{ matrix.platform_slug }}-${{ matrix.container_id }}.xml"
            TESTS_JSON='${{ toJson(matrix.tests) }}'
            TEST_COUNT=$(echo "${TESTS_JSON}" | jq 'length' | tr -d '\r')
            FAILURES=0
            CASES=""
            for i in $(seq 0 $((TEST_COUNT - 1))); do
              TEST_NAME=$(echo "${TESTS_JSON}" | jq -r ".[$i].name" | tr -d '\r')
              TEST_KIND=$(echo "${TESTS_JSON}" | jq -r ".[$i].kind" | tr -d '\r')
              START=$(date +%s)
              RC=0
              if [ "${TEST_KIND}" = "command" ]; then
                TEST_CMD=$(echo "${TESTS_JSON}" | jq -r ".[$i].command" | tr -d '\r')
                {OCX_TEST} package test --platform {TEST_PLATFORM} --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} -- \
                  ${{ matrix.shell }} -c "${TEST_CMD}" || RC=$?
              elif [ "${TEST_KIND}" = "script" ]; then
                TEST_SCRIPT=$(echo "${TESTS_JSON}" | jq -r ".[$i].script" | tr -d '\r')
                {OCX_TEST} package test --platform {TEST_PLATFORM} --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} \
                  --script "${TEST_SCRIPT}" || RC=$?
              else
                TEST_INLINE=$(echo "${TESTS_JSON}" | jq -r ".[$i].script_inline" | tr -d '\r')
                printf '%s' "${TEST_INLINE}" | {OCX_TEST} package test --platform {TEST_PLATFORM} --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} \
                  --script - || RC=$?
              fi
              END=$(date +%s)
              DUR=$((END - START))
              if [ "${RC}" -eq 0 ]; then
                CASES="${CASES}    <testcase name=\"${TEST_NAME}\" classname=\"${VERSION}.${{ matrix.platform_slug }}.${{ matrix.container_id }}\" time=\"${DUR}\"/>\n"
              else
                CASES="${CASES}    <testcase name=\"${TEST_NAME}\" classname=\"${VERSION}.${{ matrix.platform_slug }}.${{ matrix.container_id }}\" time=\"${DUR}\"><failure type=\"NonZeroExit\" message=\"exit ${RC}\"/></testcase>\n"
                FAILURES=$((FAILURES + 1))
              fi
            done
            {
              echo '<?xml version="1.0" encoding="UTF-8"?>'
              echo "<testsuites>"
              echo "  <testsuite name=\"${VERSION}.${{ matrix.platform_slug }}.${{ matrix.container_id }}\" tests=\"${TEST_COUNT}\" failures=\"${FAILURES}\">"
              if [ -n "${CI_JOB_URL:-}" ]; then
                echo "    <properties>"
                echo "      <property name=\"ci.job.url\" value=\"${CI_JOB_URL}\"/>"
                echo "    </properties>"
              fi
              printf '%b' "${CASES}"
              echo "  </testsuite>"
              echo "</testsuites>"
            } > "${JUNIT_FILE}"
            echo "wrote ${JUNIT_FILE} (tests=${TEST_COUNT}, failures=${FAILURES})"
            if [ "${FAILURES}" -gt 0 ]; then
              exit 1
            fi
"#;
    body.replace("{CONTAINER_PRELUDE}", container_prelude)
        // After `{CONTAINER_PRELUDE}` — the placeholder lives inside it.
        .replace("{CONTAINER_SETUP_BUILD}", container_setup_build)
        .replace("{METADATA_SIBLING}", metadata_sibling)
        .replace("{OCX_TEST}", ocx_test)
        .replace("{TEST_TARGET}", test_target)
        .replace("{TEST_PLATFORM}", test_platform)
}

/// The `discover` job's plan-artifact `path:`, source-dependent.
///
/// A `pypi` source derives one PEP 751 lock per discovered version during the
/// plan phase (`pipeline plan` writes them to `./locks`), and `prepare` needs
/// those locks as much as it needs `plan.json` — carrying both in the one
/// artifact is what keeps a prepare leg from re-deriving. Every other source,
/// `pylock` included (its lock is committed in the repository), uploads the
/// single file it always did.
pub fn plan_artifact_path(is_pypi: bool) -> &'static str {
    if is_pypi {
        "|\n            plan.json\n            locks/"
    } else {
        "plan.json"
    }
}

/// The `actions/upload-artifact` step header exactly as the template pins it.
///
/// [`derived_locks_artifact`] is a step the template cannot carry — it exists
/// for one source type only — and a pin written into Rust would sit outside the
/// Renovate customManager, which scans `templates/*.yml`. Reading the
/// template's own line keeps both uploads on the one bot-bumped action. The
/// fallback is inert while the template carries an upload step, which
/// `the_derived_locks_upload_tracks_the_templates_action_pin` asserts.
pub fn upload_artifact_uses() -> &'static str {
    WORKFLOW_TEMPLATE
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("- uses: actions/upload-artifact@"))
        .unwrap_or("- uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1")
}

/// The long-retention copy of a `pypi` source's derived locks, or nothing.
///
/// The `plan` artifact expires after a day, and the locks it carries are the
/// exact resolution every published env package was built from — one 90-day
/// copy is what makes a past publish reconstructable
/// (`adr_pypi_lock_derivation.md`). `if-no-files-found: ignore` because a run
/// that discovers no new version derives no lock and must not red on the empty
/// directory.
pub fn derived_locks_artifact(is_pypi: bool) -> String {
    if !is_pypi {
        return String::new();
    }
    format!(
        r#"      # Long-retention copy of the derived locks, for audit once the 1-day
      # plan artifact has expired (see adr_pypi_lock_derivation.md).
      {uses}
        with:
          name: derived-locks
          path: locks/
          retention-days: 90
          if-no-files-found: ignore
"#,
        uses = upload_artifact_uses(),
    )
}

/// The prepare job's artifact-gathering script (10-space indent, emitted into
/// that job's `run:` block).
///
/// Archive legs flatten every per-platform `bundle.tar.xz` and its metadata
/// sibling into one `bundles/` namespace keyed by `bundle-{V}-{slug}`. An env
/// source has no such file: `pipeline prepare` writes a version subtree whose
/// `env-manifest.json` names its metadata and layers by paths relative to that
/// directory, so the subtree travels whole and both the test job and
/// `pipeline push`'s `enumerate_env_manifests` resolve those paths against
/// `bundles/{V}/`.
pub fn prepare_flatten_script(is_env: bool) -> &'static str {
    if is_env {
        r#"          # The prepared env subtree travels whole: env-manifest.json names its
          # metadata and layers relative to this version directory.
          V="${{ matrix.version.version }}"
          # Same `+` → `_` translation the archive path makes: `pipeline prepare`
          # names its on-disk version directory with the OCI-tag-safe slug while
          # the matrix value keeps the build separator.
          V_SLUG="${V//+/_}"
          mkdir -p bundles
          if [ -d ".ocx-mirror/${V_SLUG}" ]; then
            cp -R ".ocx-mirror/${V_SLUG}" "bundles/${V}"
          fi"#
    } else {
        r#"          # Flatten .ocx-mirror/{V}/{P}/bundle.tar.xz → bundles/bundle-{V}-{P_slug}.tar.xz
          # and copy the per-platform metadata.json written by `pipeline prepare`
          # as sibling so `ocx package test` auto-discovers the correct override
          # (e.g. metadata-darwin.json baked content) via its bundle→metadata
          # sibling convention. Do NOT copy the spec-level metadata.json from CWD
          # — that always contains the default path, not the platform override.
          V="${{ matrix.version.version }}"
          # `pipeline prepare` normalises the build separator `+` → `_` when
          # naming its on-disk version directory (OCI-tag safe slug); the
          # matrix value still carries the original `+`, so translate before
          # globbing into the platform tree.
          V_SLUG="${V//+/_}"
          mkdir -p bundles
          shopt -s nullglob
          for platform_dir in ".ocx-mirror/${V_SLUG}"/*/; do
            [ -d "${platform_dir}" ] || continue
            P_SLUG=$(basename "${platform_dir}")
            cp "${platform_dir}bundle.tar.xz" "bundles/bundle-${V}-${P_SLUG}.tar.xz"
            cp "${platform_dir}metadata.json" "bundles/bundle-${V}-${P_SLUG}-metadata.json"
          done"#
    }
}

/// Resolve this leg's package under test, per version (12-space indent, emitted
/// immediately before the test-run steps inside the test job's version loop).
///
/// Archive legs name one bundle path. Env legs read the version's
/// `env-manifest.json` and set `METADATA` + `LAYERS` for the
/// `-m <metadata> <layers…>` form, picking the entry that matches this leg's
/// libc: a musl container leg tests the musl env of a dual-libc package, while
/// both legs of a single-env package see its one featureless entry.
///
/// The jq resolution is deliberately guarded (`2>/dev/null || true`, `// empty`)
/// so a genuine miss — a version whose prepare leg failed and uploaded no
/// manifest — reds that one version attributably through the
/// `ocx package test … || RC=$?` capture (empty METADATA → ocx fails → one JUnit
/// `<failure>`), instead of a bare jq exit tripping `set -e` and aborting every
/// remaining version with no JUnit written at all.
///
/// Every jq capture ends in `tr -d '\r'`: on windows-latest the captured value
/// otherwise keeps a trailing CR (Git Bash word-splits `$()` on LF only), which
/// reaches `ocx package test` as part of the path and fails it with os error
/// 123. `| tr -d '\r'` sits inside the pipeline so `|| true` still swallows a
/// genuine miss — `pipefail` propagates jq's exit through it unchanged.
pub fn test_target_resolve_script(is_env: bool) -> &'static str {
    if is_env {
        r#"            VERSION_DIR="bundles/${VERSION}"
            # The leg's own libc, declared on `--platform` as an os_feature so
            # dependency resolution (the env's private interpreter) can select a
            # per-libc index entry. Native legs declare no libc and stay bare.
            TEST_PLATFORM="${{ matrix.platform }}"
            case "${TEST_PLATFORM}" in
              # A platform key that already declares its own os.features is
              # authoritative — appending a second one would not even parse.
              *+*) ;;
              *)
                case "${{ matrix.container_libc }}" in
                  musl) TEST_PLATFORM="${TEST_PLATFORM}+libc.musl" ;;
                  gnu) TEST_PLATFORM="${TEST_PLATFORM}+libc.glibc" ;;
                esac
                ;;
            esac
            ENV_JSON=$(jq -c --arg p "${{ matrix.platform }}" --arg full "${TEST_PLATFORM}" '([.envs[] | select(.platform == $full)] + [.envs[] | select(.platform == $p)]) | first // empty' "${VERSION_DIR}/env-manifest.json" 2>/dev/null | tr -d '\r' || true)
            METADATA="${VERSION_DIR}/$(printf '%s' "${ENV_JSON}" | jq -r '.metadata_path // empty' 2>/dev/null | tr -d '\r' || true)"
            LAYERS=""
            for rel in $(printf '%s' "${ENV_JSON}" | jq -r '.layers[].path // empty' 2>/dev/null | tr -d '\r' || true); do
              LAYERS="${LAYERS} ${VERSION_DIR}/${rel}"
            done"#
    } else {
        r#"            BUNDLE="bundles/bundle-${VERSION}-${{ matrix.platform_slug }}.tar.xz""#
    }
}
