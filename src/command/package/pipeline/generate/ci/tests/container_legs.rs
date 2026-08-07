// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use std::path::Path;
use tempfile::tempdir;

// ── Container test legs ───────────────────────────────────────────────

#[test]
fn container_matrix_entries_stay_valid_yaml() {
    // Container mode is the only path that emits extra matrix keys, so it is
    // the only path that can break the hand-built indentation of the
    // `include:` block. A string assertion cannot see that; parsing can.
    for fixture in [
        "mirror-multi-container.yml",
        "mirror-container-mixed.yml",
        "mirror-container-libc.yml",
        "mirror-container-setup.yml",
    ] {
        let workflow = workflow_for(fixture);
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&workflow)
            .unwrap_or_else(|e| panic!("{fixture} must render parseable YAML: {e}\n{workflow}"));

        let legs = parsed["jobs"]["test"]["strategy"]["matrix"]["include"]
            .as_sequence()
            .unwrap_or_else(|| panic!("{fixture}: test matrix must be a sequence"));
        // Every container leg must carry both keys the run step reads; a leg
        // with an image but no libc would build a bogus ocx triple.
        let with_image = legs
            .iter()
            .filter(|leg| leg.get("container_image").is_some())
            .inspect(|leg| {
                assert!(
                    leg.get("container_libc").is_some(),
                    "{fixture}: a leg with container_image must also carry container_libc"
                );
            })
            .count();
        assert!(with_image > 0, "{fixture} must render container legs");
    }
}

#[test]
fn container_setup_builds_the_image_once_per_leg() {
    let workflow = workflow_for("mirror-container-setup.yml");

    for needle in [
        // The Dockerfile crosses into the shell through `env:`, never as an
        // inline expression — that is what makes quotes and newlines safe.
        "OCX_CONTAINER_DOCKERFILE: ${{ matrix.container_dockerfile }}",
        // Once per leg, not once per version: every version after the first
        // finds the tag already built.
        r#"if ! docker image inspect "${OCX_SETUP_TAG}" >/dev/null 2>&1; then"#,
        r#"| docker build --platform "${{ matrix.docker_platform }}" -t "${OCX_SETUP_TAG}" - \"#,
        // Without this the provisioned image is built and then ignored.
        r#"CONTAINER_IMAGE="${OCX_SETUP_TAG}""#,
        // A failing setup command must name itself in the run summary; a
        // bare non-zero `docker build` reads as a renderer bug.
        "::error::container setup failed for",
    ] {
        assert!(
            workflow.contains(needle),
            "a setup-declaring spec must render `{needle}`, got:\n{workflow}"
        );
    }
}

#[test]
fn the_container_image_is_pulled_with_retries_before_anything_runs_it() {
    // Left to `docker run`, a rate-limited pull surfaces in the JUnit report
    // as a failed testcase — the one thing a red testcase must not be able
    // to mean. Pulling up front makes it a failed step instead. The setup
    // fixture renders both consumers of the image (`docker build`'s FROM and
    // `docker run`), so it is the one that can prove the ordering.
    let workflow = workflow_for("mirror-container-setup.yml");

    let pull = r#"until docker pull --platform "${{ matrix.docker_platform }}" "${CONTAINER_IMAGE}"; do"#;
    let pull_at = workflow.find(pull).unwrap_or_else(|| {
        panic!("container legs must pull the image explicitly with the docker platform, got:\n{workflow}")
    });

    // A bare pull would only move the flake, so assert the whole loop: the
    // once-per-leg guard (without it every version spends a manifest
    // request — the resource being rate-limited), five attempts, doubling
    // delay, and a hard exit once they are spent.
    for needle in [
        r#"if ! docker image inspect "${CONTAINER_IMAGE}" >/dev/null 2>&1; then"#,
        "OCX_PULL_DELAY=2",
        r#"if [ "${OCX_PULL_ATTEMPT}" -ge 5 ]; then"#,
        r#"sleep "${OCX_PULL_DELAY}""#,
        "OCX_PULL_DELAY=$((OCX_PULL_DELAY * 2))",
        "::error::could not pull ${CONTAINER_IMAGE}",
    ] {
        assert!(
            workflow.contains(needle),
            "the pull must retry with backoff and fail the job when spent — missing `{needle}`, got:\n{workflow}"
        );
    }

    // Both consumers go to the network on a cache miss, so both must come
    // after the pull — otherwise the retry protects nothing.
    for consumer in ["docker build --platform", "docker run --rm -i --platform"] {
        let consumer_at = workflow
            .find(consumer)
            .unwrap_or_else(|| panic!("the setup fixture must render `{consumer}`, got:\n{workflow}"));
        assert!(
            pull_at < consumer_at,
            "the retrying pull must precede `{consumer}`, got:\n{workflow}"
        );
    }
}

#[test]
fn container_setup_matrix_entries_stay_valid_yaml() {
    // Asserting the parsed value, not the rendered text: it is the only way
    // to prove the block scalar's indentation, the honoured shell, the
    // one-RUN-per-command shape and the survival of both quote flavours in
    // a single check — and each of those is a way to emit a broken image.
    let workflow = workflow_for("mirror-container-setup.yml");
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&workflow)
        .unwrap_or_else(|e| panic!("setup fixture must render parseable YAML: {e}\n{workflow}"));
    let legs = parsed["jobs"]["test"]["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("test matrix must be a sequence");
    let leg = |id: &str| {
        legs.iter()
            .find(|leg| leg["container_id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("no leg with container_id {id}"))
    };

    assert_eq!(
        leg("alpine_3_20")["container_dockerfile"].as_str(),
        Some("FROM alpine:3.20\nSHELL [\"sh\", \"-c\"]\nRUN apk add --no-cache libstdc++\n"),
    );
    assert_eq!(
        leg("ubuntu_24_04")["container_dockerfile"].as_str(),
        Some(concat!(
            "FROM ubuntu:24.04\n",
            "SHELL [\"bash\", \"-c\"]\n",
            "RUN apt-get update && apt-get install -y --no-install-recommends libatomic1\n",
            "RUN sh -c 'echo \"provisioned\" > /etc/ocx-setup-marker'\n",
        )),
    );
    // Same platform, no `setup:` — the key set stays what it was.
    assert!(
        leg("fedora_40").get("container_dockerfile").is_none(),
        "a container without setup must not gain a container_dockerfile key",
    );
}

#[test]
fn a_container_spec_without_setup_emits_no_setup_machinery() {
    // The container half of the byte-identical proof (the golden corpus is
    // the native half): container mode predates `setup:`, so a spec that
    // declares none must render exactly what it rendered before.
    let workflow = workflow_for("mirror-multi-container.yml");

    for needle in [
        "container_dockerfile",
        "OCX_CONTAINER_DOCKERFILE",
        "docker build",
        "ocx-mirror-setup",
    ] {
        assert!(
            !workflow.contains(needle),
            "a spec without setup must not render `{needle}`, got:\n{workflow}"
        );
    }
}

/// Render a fixture and parse one of its generated workflows.
fn parse_workflow(fixture: &str, name: &str) -> serde_yaml_ng::Value {
    let dir = tempdir().unwrap();
    render_fixture(fixture, dir.path()).unwrap_or_else(|e| panic!("{fixture} must render: {e}"));
    let rendered = std::fs::read_to_string(dir.path().join(".github/workflows").join(name)).unwrap();
    serde_yaml_ng::from_str(&rendered).unwrap_or_else(|e| panic!("{name} must be parseable YAML: {e}\n{rendered}"))
}

#[test]
fn patch_is_dispatch_only_and_exposes_the_whole_selection_surface() {
    // Patching re-emits published manifests, so anything but an explicit
    // human dispatch — a schedule, a push, a wire to `plan`'s has_drift —
    // would republish the corpus on somebody else's timetable.
    let parsed = parse_workflow("mirror-ghcr-announce.yml", "patch.yml");

    let triggers = &parsed["on"];
    assert_eq!(
        triggers
            .as_mapping()
            .unwrap_or_else(|| panic!("triggers must be a mapping, got: {triggers:?}"))
            .keys()
            .map(|k| k.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["workflow_dispatch"],
        "patch must be dispatch-only — no push, no schedule",
    );

    // The command's selection surface is these three flags; an input the
    // workflow does not expose is a patch a maintainer can only run from a
    // laptop, which is the gap this workflow closes.
    let inputs = &triggers["workflow_dispatch"]["inputs"];
    assert_eq!(
        inputs
            .as_mapping()
            .unwrap_or_else(|| panic!("inputs must be a mapping, got: {inputs:?}"))
            .keys()
            .map(|k| k.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["version", "min_version", "max_version"],
    );

    // Naming any permission sets every unnamed one to `none`, so this map is
    // the job's whole token. `packages: write` re-emits the manifests;
    // `contents: read` is checkout + setup-ocx. The push job's `actions`,
    // `checks` and `pull-requests` scopes buy steps this job does not run.
    let permissions = &parsed["jobs"]["patch"]["permissions"];
    assert_eq!(
        permissions
            .as_mapping()
            .unwrap_or_else(|| panic!("a ghcr patch job must name its permissions, got: {permissions:?}"))
            .iter()
            .map(|(k, v)| (k.as_str().unwrap(), v.as_str().unwrap()))
            .collect::<Vec<_>>(),
        vec![("contents", "read"), ("packages", "write")],
    );

    // A successful patch chains into announce, which authenticates against
    // the index repository with this secret and nothing else.
    assert_eq!(
        patch_step(&parsed)["env"]["OCX_ANNOUNCE_TOKEN"].as_str(),
        Some("${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
    );
}

/// The `patch.yml` step that runs the command.
fn patch_step(parsed: &serde_yaml_ng::Value) -> &serde_yaml_ng::Value {
    parsed["jobs"]["patch"]["steps"]
        .as_sequence()
        .expect("patch job must have steps")
        .iter()
        .find(|step| step.get("env").is_some())
        .expect("one step must carry the patch environment")
}

/// Run `patch.yml`'s command step against a stub `ocx-mirror`, returning the
/// argv it was invoked with.
///
/// The step body carries no `${{ }}` — every dispatch input reaches it
/// through `env:` — so it is the actual shell GitHub would run, not a
/// paraphrase of it. Asserting on the script text instead would only prove
/// the template says what the template says.
fn patch_argv(inputs: &[(&str, &str)]) -> Vec<String> {
    let parsed = parse_workflow("mirror-ghcr-announce.yml", "patch.yml");
    let script = patch_step(&parsed)["run"]
        .as_str()
        .expect("the step must carry a run block");

    let dir = tempdir().unwrap();
    let argv_file = dir.path().join("argv");
    let stub = dir.path().join("ocx-mirror");
    std::fs::write(
        &stub,
        format!(
            "#!/usr/bin/env bash\nfor arg in \"$@\"; do printf '%s\\n' \"${{arg}}\" >> {}; done\n",
            argv_file.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let script_path = dir.path().join("step.sh");
    std::fs::write(&script_path, script).unwrap();

    let mut command = std::process::Command::new("bash");
    command
        .arg(&script_path)
        .current_dir(dir.path())
        .env(
            "PATH",
            format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()),
        )
        // GitHub always sets a dispatch input's env var; an omitted input
        // arrives as the empty string, never as an absent variable.
        .env("VERSIONS", "")
        .env("MIN_VERSION", "")
        .env("MAX_VERSION", "");
    for (key, value) in inputs {
        command.env(key, value);
    }

    let output = command.output().expect("the step script must run under bash");
    assert!(
        output.status.success(),
        "the step script exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    std::fs::read_to_string(&argv_file)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn an_empty_patch_dispatch_names_no_selection_at_all() {
    // The documented default is "patch everything published", and it is
    // reached by passing no selection flag. A `--min-version ""` emitted for
    // an omitted input would silently narrow the run instead — a maintainer
    // who dispatched with empty fields would get a subset and no sign of it.
    assert_eq!(patch_argv(&[]), vec!["package", "pipeline", "patch", "--metadata-only"],);
}

#[test]
fn patch_dispatch_inputs_become_the_command_line_selection() {
    // `--version` repeats on the CLI but a dispatch input is one string, so
    // the step splits it. Both separators, and a run of them, resolve to one
    // flag per version; the bounds pass through as themselves.
    assert_eq!(
        patch_argv(&[
            ("VERSIONS", "3.29.0, 3.28.0 3.27.0"),
            ("MIN_VERSION", "3.0.0"),
            ("MAX_VERSION", "4.0.0"),
        ]),
        vec![
            "package",
            "pipeline",
            "patch",
            "--metadata-only",
            "--version",
            "3.29.0",
            "--version",
            "3.28.0",
            "--version",
            "3.27.0",
            "--min-version",
            "3.0.0",
            "--max-version",
            "4.0.0",
        ],
    );

    // A bound on its own must not drag an empty `--version` along with it.
    assert_eq!(
        patch_argv(&[("MIN_VERSION", "3.0.0")]),
        vec![
            "package",
            "pipeline",
            "patch",
            "--metadata-only",
            "--min-version",
            "3.0.0"
        ],
    );
}

#[test]
fn announce_from_registry_is_dispatch_only_by_default_and_carries_the_token() {
    // The Python acceptance test only text-greps `"on:"` (the locked test
    // env has no yaml module), so the real parse lives here. A push trigger
    // on this workflow would open an index pull request on every commit —
    // the one thing it must never do. A schedule is opt-in per spec, so a
    // spec that did not ask for one gets neither.
    let dir = tempdir().unwrap();
    render_fixture("mirror-ghcr-announce.yml", dir.path()).expect("announce fixture must render");
    let rendered = std::fs::read_to_string(dir.path().join(".github/workflows/announce-from-registry.yml")).unwrap();

    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&rendered)
        .unwrap_or_else(|e| panic!("announce-from-registry.yml must be parseable YAML: {e}\n{rendered}"));

    let triggers = &parsed["on"];
    let mapping = triggers
        .as_mapping()
        .unwrap_or_else(|| panic!("triggers must be a mapping, got: {triggers:?}"));
    assert_eq!(
        mapping.keys().map(|k| k.as_str().unwrap()).collect::<Vec<_>>(),
        vec!["workflow_dispatch"],
        "a spec that named no announce schedule must get a dispatch-only workflow — and never a push trigger",
    );

    let dry_run = &triggers["workflow_dispatch"]["inputs"]["dry_run"];
    assert_eq!(dry_run["type"].as_str(), Some("boolean"), "got: {dry_run:?}");
    assert_eq!(dry_run["default"].as_bool(), Some(true), "got: {dry_run:?}");

    // The announce cannot open a pull request without the secret, so an
    // env: block that lost it would turn every dispatch into an auth error.
    let step = parsed["jobs"]["announce"]["steps"]
        .as_sequence()
        .expect("announce job must have steps")
        .iter()
        .find(|step| step.get("env").is_some())
        .expect("one step must carry the announce environment");
    assert_eq!(
        step["env"]["OCX_ANNOUNCE_TOKEN"].as_str(),
        Some("${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
        "got: {step:?}",
    );
}

/// Parse one spec's rendered `announce-from-registry.yml`, from an inline
/// spec at the root.
fn announce_from_registry_of(yaml: &str) -> serde_yaml_ng::Value {
    let rendered = render_announce_from_registry(&spec_from_yaml(yaml), &root_slot());
    serde_yaml_ng::from_str(&rendered)
        .unwrap_or_else(|e| panic!("announce-from-registry.yml must be parseable YAML: {e}\n{rendered}"))
}

/// `SHFMT_SPEC` with an `announce:` block, optionally on a timer.
fn shfmt_announcing(schedule: Option<&str>) -> String {
    let cron = schedule
        .map(|cron| format!("  schedule: \"{cron}\"\n"))
        .unwrap_or_default();
    format!("{SHFMT_SPEC}announce:\n  package: mvdan/shfmt\n  fork: ocx-contrib/index\n{cron}")
}

/// The keys of a rendered workflow's `on:` block, in order.
fn trigger_keys(parsed: &serde_yaml_ng::Value) -> Vec<&str> {
    parsed["on"]
        .as_mapping()
        .unwrap_or_else(|| panic!("triggers must be a mapping, got: {:?}", parsed["on"]))
        .keys()
        .map(|key| key.as_str().expect("a trigger key is a string"))
        .collect()
}

#[test]
fn an_announce_schedule_adds_a_cron_trigger_beside_the_dispatch() {
    // The opt-in half: an operator who wants the catch-up unattended gets a
    // timer, and keeps the manual dispatch they had.
    let parsed = announce_from_registry_of(&shfmt_announcing(Some("23 5 * * 2")));
    assert_eq!(
        trigger_keys(&parsed),
        vec!["schedule", "workflow_dispatch"],
        "a schedule is added to the dispatch, never a push trigger and never instead of it",
    );
    assert_eq!(
        parsed["on"]["schedule"][0]["cron"].as_str(),
        Some("23 5 * * 2"),
        "got: {:?}",
        parsed["on"]["schedule"],
    );

    // Two separate opt-ins on two separate workflows: an operator who wants
    // unattended announces has not asked for unattended tag repair, and the
    // shared `schedule_block` helper makes crossing them a one-line typo.
    assert_eq!(
        trigger_keys(&cascade_of(&shfmt_announcing(Some("23 5 * * 2")))),
        vec!["workflow_dispatch"],
        "announce.schedule must not put the repair workflow on a timer",
    );
    assert_eq!(
        trigger_keys(&announce_from_registry_of(&format!(
            "{}cascade:\n  schedule: \"17 4 * * 1\"\n",
            shfmt_announcing(None)
        ))),
        vec!["workflow_dispatch"],
        "cascade.schedule must not put the announce workflow on a timer",
    );
}

#[test]
fn the_announce_step_answers_dry_run_for_a_scheduled_event() {
    // `inputs.dry_run` is empty outside a dispatch. Left alone it reads as
    // "not true", which is the right answer by accident — one that a
    // default flip would silently invert.
    let step = announce_from_registry_of(&shfmt_announcing(None))["jobs"]["announce"]["steps"]
        .as_sequence()
        .expect("announce job must have steps")
        .iter()
        .find(|step| step["name"].as_str() == Some("Announce every registry tag into the index"))
        .expect("the announce step must be named")
        .clone();
    assert_eq!(
        step["env"]["DRY_RUN"].as_str(),
        Some("${{ github.event_name == 'schedule' && 'false' || inputs.dry_run }}"),
        "got: {step:?}",
    );
}

#[test]
fn cascade_workflow_is_dispatch_only_by_default_and_carries_the_token() {
    // A push trigger here would re-point published rolling tags on every
    // commit, which is the one thing a repair must never do. A schedule is
    // opt-in per spec, so a spec that did not ask for one gets neither.
    // `dry_run` defaulting to true is the other half: a dispatch that names
    // nothing audits.
    let dir = tempdir().unwrap();
    render_fixture("mirror-ghcr-announce.yml", dir.path()).expect("announce fixture must render");
    let rendered = std::fs::read_to_string(dir.path().join(".github/workflows/cascade.yml")).unwrap();

    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&rendered)
        .unwrap_or_else(|e| panic!("cascade.yml must be parseable YAML: {e}\n{rendered}"));

    let triggers = &parsed["on"];
    let mapping = triggers
        .as_mapping()
        .unwrap_or_else(|| panic!("triggers must be a mapping, got: {triggers:?}"));
    assert_eq!(
        mapping.keys().map(|k| k.as_str().unwrap()).collect::<Vec<_>>(),
        vec!["workflow_dispatch"],
        "a spec that named no schedule must get a dispatch-only cascade — and never a push trigger",
    );

    let dry_run = &triggers["workflow_dispatch"]["inputs"]["dry_run"];
    assert_eq!(dry_run["type"].as_str(), Some("boolean"), "got: {dry_run:?}");
    assert_eq!(dry_run["default"].as_bool(), Some(true), "got: {dry_run:?}");

    // A repaired alias points at a digest the index does not know, so the
    // run ends by announcing — an env: block that lost the secret would
    // degrade every repair to a silent notice.
    let step = parsed["jobs"]["cascade"]["steps"]
        .as_sequence()
        .expect("cascade job must have steps")
        .iter()
        .find(|step| step.get("env").is_some())
        .expect("one step must carry the announce environment");
    assert_eq!(
        step["env"]["OCX_ANNOUNCE_TOKEN"].as_str(),
        Some("${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
        "got: {step:?}",
    );

    // The repair writes tags to the target repository — the read scope
    // discover gets would 403 the moment it moved one.
    assert!(
        rendered.contains("      packages: write\n"),
        "a ghcr target's repair needs packages: write, got:\n{rendered}"
    );
}

#[test]
fn cascade_workflow_follows_the_spec_flag() {
    // No cascade, no rolling alias, nothing to repair — and a workflow that
    // dispatched anyway would report findings on a graph the spec never
    // asked for.
    let cascading = render_spec(&spec_from_yaml(SHFMT_SPEC), &root_slot());
    assert!(
        cascading.contains_key(Path::new(".github/workflows/cascade.yml")),
        "cascade defaults to true, so the workflow is emitted by default"
    );

    let plain = render_spec(&spec_from_yaml(&format!("{SHFMT_SPEC}cascade: false\n")), &root_slot());
    assert!(
        !plain.contains_key(Path::new(".github/workflows/cascade.yml")),
        "a spec that publishes no rolling tag must not get a repair workflow"
    );

    // The map form is an enabled cascade with a trigger attached, so it
    // emits the same workflow the bare `true` does.
    let scheduled = render_spec(
        &spec_from_yaml(&format!("{SHFMT_SPEC}cascade:\n  schedule: \"17 4 * * 1\"\n")),
        &root_slot(),
    );
    assert!(
        scheduled.contains_key(Path::new(".github/workflows/cascade.yml")),
        "a spec naming a cascade schedule is a cascading spec"
    );
}

/// Parse one spec's rendered `cascade.yml`, from an inline spec at the root.
fn cascade_of(yaml: &str) -> serde_yaml_ng::Value {
    let rendered = render_cascade(&spec_from_yaml(yaml), &root_slot());
    serde_yaml_ng::from_str(&rendered).unwrap_or_else(|e| panic!("cascade.yml must be parseable YAML: {e}\n{rendered}"))
}

#[test]
fn a_cascade_schedule_adds_a_cron_trigger_beside_the_dispatch() {
    // The opt-in half: an operator who wants unattended repair gets a
    // timer, and keeps the manual dispatch they had.
    let parsed = cascade_of(&format!("{SHFMT_SPEC}cascade:\n  schedule: \"17 4 * * 1\"\n"));

    let triggers = parsed["on"]
        .as_mapping()
        .unwrap_or_else(|| panic!("triggers must be a mapping, got: {:?}", parsed["on"]));
    assert_eq!(
        triggers.keys().map(|k| k.as_str().unwrap()).collect::<Vec<_>>(),
        vec!["schedule", "workflow_dispatch"],
        "a schedule is added to the dispatch, never a push trigger and never instead of it",
    );
    assert_eq!(
        parsed["on"]["schedule"][0]["cron"].as_str(),
        Some("17 4 * * 1"),
        "got: {:?}",
        parsed["on"]["schedule"],
    );
}

#[test]
fn the_repair_step_answers_dry_run_for_a_scheduled_event() {
    // `inputs.dry_run` is empty outside a dispatch. Left alone it reads as
    // "not true", which is the right answer by accident — one that a
    // default flip would silently invert.
    let step = cascade_of(SHFMT_SPEC)["jobs"]["cascade"]["steps"]
        .as_sequence()
        .expect("cascade job must have steps")
        .iter()
        .find(|step| step["name"].as_str() == Some("Repair the rolling-tag cascade"))
        .expect("the repair step must be named")
        .clone();
    assert_eq!(
        step["env"]["DRY_RUN"].as_str(),
        Some("${{ github.event_name == 'schedule' && 'false' || inputs.dry_run }}"),
        "got: {step:?}",
    );
}

#[test]
fn cascade_queues_behind_its_own_specs_publish_workflow() {
    // A repair and a live push both re-point the same rolling aliases, so
    // the two must not interleave. The cascade workflow has no runtime
    // handle on the push workflow's name, so its group is a baked literal —
    // derive both ends from one render or it drifts unnoticed.
    let nested = SHFMT_SPEC.replace("name: shfmt", "name: shfmt-py3.13");
    let files = render(&[
        (root_slot(), spec_from_yaml(SHFMT_SPEC)),
        (slot_at("py3.13/mirror.yml"), spec_from_yaml(&nested)),
    ]);

    let parse = |relative: String| -> serde_yaml_ng::Value {
        let rendered = &files[Path::new(&relative)];
        serde_yaml_ng::from_str(rendered).unwrap_or_else(|e| panic!("{relative} must parse: {e}\n{rendered}"))
    };

    let mut groups = Vec::new();
    for suffix in ["", "-py3.13"] {
        let push = parse(format!(".github/workflows/mirror{suffix}.yml"));
        let cascade = parse(format!(".github/workflows/cascade{suffix}.yml"));

        assert_eq!(
            push["concurrency"]["group"].as_str(),
            Some("mirror-${{ github.workflow }}-publish"),
            "the literal baked into cascade{suffix}.yml is only correct while the push group reads this way",
        );
        let name = push["name"].as_str().expect("the push workflow must be named");
        let group = cascade["concurrency"]["group"]
            .as_str()
            .unwrap_or_else(|| panic!("cascade{suffix}.yml must name a concurrency group"));
        assert_eq!(
            group,
            format!("mirror-{name}-publish"),
            "cascade{suffix}.yml must queue behind the workflow named {name}",
        );
        assert_eq!(
            cascade["concurrency"]["cancel-in-progress"].as_bool(),
            Some(false),
            "a repair cancelled mid-flight leaves the graph it was fixing half-written",
        );
        groups.push(group.to_string());
    }
    assert_ne!(
        groups[0], groups[1],
        "a nested spec must join its own publish group, not the root spec's",
    );
}

#[test]
fn container_legs_execute_the_artifact_inside_the_image() {
    // The gate this feature exists for: an `os.features` musl/glibc claim is
    // only verified when the mirrored binary is executed by that image's
    // loader. A matrix that merely names images proves nothing, so assert
    // the wrapper actually runs `ocx package test` inside `docker run`.
    let workflow = workflow_for("mirror-multi-container.yml");

    assert!(
        workflow.contains("docker run --rm -i --platform \"${{ matrix.docker_platform }}\" \\"),
        "container legs must invoke docker run, got:\n{workflow}"
    );
    assert!(
        workflow.contains("\"${CONTAINER_IMAGE}\" ocx \"$@\""),
        "docker run must exec ocx inside the image, got:\n{workflow}"
    );
    // Every test kind routes through the wrapper, not the runner's ocx.
    assert_eq!(
        workflow.matches("ocx_test package test --platform").count(),
        3,
        "all three test kinds must run through the container wrapper, got:\n{workflow}"
    );
    assert!(
        !workflow.contains(" ocx package test --platform"),
        "no test may bypass the wrapper onto the runner's ocx, got:\n{workflow}"
    );
    // The workspace must be mounted at its own path, or the bundle and its
    // `-metadata.json` sibling resolve to nothing inside the container.
    assert!(
        workflow.contains("-v \"${GITHUB_WORKSPACE}:${GITHUB_WORKSPACE}\" -w \"${GITHUB_WORKSPACE}\""),
        "workspace must be mounted at its own path, got:\n{workflow}"
    );
    assert!(
        workflow.contains("-v \"${OCX_CONTAINER_BIN}:/usr/local/bin/ocx:ro\""),
        "the libc-matched ocx must be mounted into the image, got:\n{workflow}"
    );
}

#[test]
fn container_legs_fetch_a_libc_matched_ocx_per_architecture() {
    // The static ocx is what runs inside the image; a gnu build on Alpine
    // dies in the loader before any test starts. The triple is assembled
    // from the leg's arch and its image's libc, so both axes must appear.
    let workflow = workflow_for("mirror-container-mixed.yml");

    assert!(
        workflow.contains("linux/amd64) OCX_ARCH=x86_64 ;;") && workflow.contains("linux/arm64) OCX_ARCH=aarch64 ;;"),
        "both linux architectures must map to an ocx triple, got:\n{workflow}"
    );
    assert!(
        workflow.contains("OCX_TRIPLE=\"${OCX_ARCH}-unknown-linux-${{ matrix.container_libc }}\""),
        "the triple must combine arch with the leg's libc, got:\n{workflow}"
    );
    // Releases ship .tar.gz (dist-workspace.toml sets unix-archive); the
    // .tar.xz spelling 404s and would silently leave the runner's own ocx.
    assert!(
        workflow.contains(&format!(
            "https://github.com/ocx-sh/ocx/releases/download/{OCX_CONTAINER_CLI_TAG}/ocx-${{OCX_TRIPLE}}.tar.gz"
        )),
        "must download the pinned ocx release as .tar.gz, got:\n{workflow}"
    );
}

#[test]
fn every_setup_ocx_step_pins_the_renderer_ocx_version() {
    // The other half of the pin the container legs get from their download
    // URL. A `setup-ocx` step without the `version:` input floats that job
    // to whatever ocx is newest the day the mirror happens to run, so one
    // missed step is enough to have the two halves of a test matrix
    // exercising different binaries. Assert per step, on every generated
    // file — the announce fixture is one of the two rendering all four.
    let dir = tempdir().unwrap();
    render_fixture("mirror-ghcr-announce.yml", dir.path()).expect("announce fixture must render");

    let expected = format!("        with:\n          version: \"{}\"", ocx_cli_version());
    let mut steps = 0;
    for entry in std::fs::read_dir(dir.path().join(".github/workflows")).unwrap() {
        let path = entry.unwrap().path();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !line.contains("uses: ocx-sh/setup-ocx@") {
                continue;
            }
            steps += 1;
            assert!(
                lines[index + 1..].join("\n").starts_with(&expected),
                "{} line {}: setup-ocx must pin the renderer's ocx version, got:\n{}",
                path.display(),
                index + 1,
                lines[index..].join("\n"),
            );
        }
    }
    // 5 in mirror.yml, 1 each in describe / patch / cascade /
    // announce-from-registry / verify-generated.
    assert_eq!(steps, 10, "every generated workflow's setup-ocx steps must be covered");
}

#[test]
fn container_libc_and_shell_follow_the_image_basename() {
    // Alpine is the only musl base in the corpus, and it ships no bash.
    // A registry-qualified reference must classify like its bare form —
    // getting this wrong hands Alpine a gnu ocx and a missing shell.
    let workflow = workflow_for("mirror-container-mixed.yml");

    assert!(
        workflow.contains("container_image: \"docker.io/library/alpine:3.20\"\n            container_libc: \"musl\"\n            docker_platform: \"linux/amd64\"\n            shell: sh\n"),
        "a registry-qualified alpine must still infer musl + sh, got:\n{workflow}"
    );
    assert!(
        workflow.contains(
            "container_image: \"debian:12\"\n            container_libc: \"gnu\"\n            docker_platform: \"linux/amd64\"\n            shell: bash\n"
        ),
        "debian must infer gnu + bash, got:\n{workflow}"
    );
}

#[test]
fn container_ids_match_the_slug_push_looks_junit_files_up_by() {
    // `pipeline push` finds each leg's result at
    // `junit-{V}-{platform_slug}-{container_id}.xml`, computing the id with
    // `spec::image_to_container_id` (dots slugified too). If the renderer
    // names the file differently every container result reads as missing and
    // the run publishes nothing while looking green.
    let workflow = workflow_for("mirror-multi-container.yml");

    for (image, expected) in [
        ("ubuntu:24.04", "ubuntu_24_04"),
        ("alpine:3.20", "alpine_3_20"),
        ("fedora:40", "fedora_40"),
    ] {
        assert_eq!(
            spec::image_to_container_id(image),
            expected,
            "slug contract with pipeline push"
        );
        assert!(
            workflow.contains(&format!("container_id: {expected}\n")),
            "matrix must carry container_id {expected}, got:\n{workflow}"
        );
    }
    assert!(
        workflow.contains(
            "JUNIT_FILE=\"junit/junit-${VERSION}-${{ matrix.platform_slug }}-${{ matrix.container_id }}.xml\""
        ),
        "the JUnit filename must be keyed by container_id, got:\n{workflow}"
    );
}

#[test]
fn a_libc_bearing_platform_key_renders_a_leg_that_can_run() {
    // The gate G-E case. `linux/amd64+libc.musl` is the only way to declare
    // a libc claim, and every part of the leg has to survive it:
    //
    //   * `docker run --platform` and the ocx-triple `case` see the bare
    //     `linux/amd64` — docker rejects the `+libc.musl` spelling outright,
    //     and the `case` used to fall through to `*)` and abort the leg.
    //   * the matrix label and `ocx package test --platform` see the FULL
    //     key, which is what disambiguates the two entries.
    //   * `platform_slug` is the name `pipeline prepare` gave the bundle, so
    //     the leg finds a file that exists.
    let workflow = workflow_for("mirror-container-libc.yml");

    assert!(
        workflow.contains(
            "          - platform: linux/amd64+libc.musl\n            platform_slug: linux_amd64_libc.musl\n"
        ),
        "the matrix label must keep the full key and slug it the way prepare does, got:\n{workflow}"
    );
    assert!(
        workflow.contains(
            "container_image: \"alpine:3.20\"\n            container_libc: \"musl\"\n            docker_platform: \"linux/amd64\"\n"
        ),
        "the docker platform must drop the os.features suffix, got:\n{workflow}"
    );
    // Everything docker parses reads docker_platform; nothing else may.
    assert!(
        workflow.contains("docker run --rm -i --platform \"${{ matrix.docker_platform }}\" \\")
            && workflow.contains("case \"${{ matrix.docker_platform }}\" in"),
        "docker must be handed the feature-stripped platform, got:\n{workflow}"
    );
    // …and the artifact selection still reads the full key.
    assert!(
        workflow.contains("ocx_test package test --platform \"${{ matrix.platform }}\""),
        "`ocx package test` must keep the full platform key, got:\n{workflow}"
    );
}

#[test]
fn the_rendered_slug_is_the_one_prepare_writes_the_bundle_under() {
    // The leg reads `bundles/bundle-{V}-{platform_slug}.tar.xz`, which the
    // workflow flattened out of `pipeline prepare`'s work tree by basename.
    // Two independent slug rules here means the leg reds on a missing
    // bundle, so assert the renderer and prepare agree — for a libc key,
    // where the naive `/`→`_` rule diverges.
    let workflow = workflow_for("mirror-container-libc.yml");
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&workflow).expect("parseable workflow");
    let legs = parsed["jobs"]["test"]["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("matrix include")
        .clone();

    for key in ["linux/amd64+libc.musl", "linux/amd64+libc.glibc"] {
        let rendered = legs
            .iter()
            .find(|leg| leg["platform"].as_str() == Some(key))
            .unwrap_or_else(|| panic!("no leg for {key} in:\n{workflow}"))["platform_slug"]
            .as_str()
            .expect("platform_slug")
            .to_owned();
        let prepared = crate::pipeline::orchestrator::task_dir(
            Path::new("/work"),
            "3.7.0",
            &key.parse::<ocx_lib::oci::Platform>().expect("valid platform"),
        );
        assert_eq!(
            rendered,
            prepared.file_name().unwrap().to_string_lossy(),
            "rendered slug for {key} must equal the basename `pipeline prepare` writes"
        );
    }
}

#[test]
fn container_legs_refuse_a_runner_of_the_wrong_architecture() {
    // No qemu is installed, so an arm64 leg on an x86_64 runner cannot
    // execute the image. Fail with the reason and the fix instead of a bare
    // docker exec-format error minutes into the run.
    let workflow = workflow_for("mirror-container-mixed.yml");

    assert!(
        workflow.contains("RUNNER_ARCH_UNAME=\"$(uname -m)\"")
            && workflow.contains("if [ \"${RUNNER_ARCH_UNAME}\" != \"${OCX_ARCH}\" ]; then"),
        "the prelude must compare the runner's arch to the leg's, got:\n{workflow}"
    );
    assert!(
        workflow.contains("set an arch-matched \\`runner:\\` on this platform"),
        "the error must name the fix, got:\n{workflow}"
    );
}

#[test]
fn native_legs_of_a_mixed_spec_keep_running_on_the_runner() {
    // A spec with containers on linux and none on darwin renders both. The
    // darwin leg carries no container keys, so `${{ matrix.container_image }}`
    // is empty there and the wrapper takes its native branch.
    let workflow = workflow_for("mirror-container-mixed.yml");

    assert!(
        workflow.contains(
            "          - platform: darwin/arm64\n            platform_slug: darwin_arm64\n            runner: macos-latest\n            container_id: _native_\n            shell: bash\n"
        ),
        "the darwin leg must stay native with no container keys, got:\n{workflow}"
    );
    assert!(
        workflow.contains("              else\n                ocx \"$@\"\n              fi"),
        "the wrapper must fall back to the runner's ocx when no image is set, got:\n{workflow}"
    );
}

#[test]
fn a_spec_without_containers_emits_no_container_machinery() {
    // The companion to the golden corpus, named so a regression says why:
    // native specs must not gain a docker prelude or container matrix keys.
    let workflow = workflow_for("mirror-minimal.yml");

    for needle in [
        "docker run",
        "container_image:",
        "container_libc:",
        "docker_platform:",
        "ocx_test",
    ] {
        assert!(
            !workflow.contains(needle),
            "native spec must not render `{needle}`, got:\n{workflow}"
        );
    }
    assert!(
        workflow.contains("container_id: _native_"),
        "native legs keep the _native_ sentinel, got:\n{workflow}"
    );
}

#[test]
fn render_full_platforms_spec_writes_workflow() {
    // §3.3: Fixture mirror-full-platforms.yml — all 6 platforms rendered.
    let dir = tempdir().unwrap();
    let result = render_fixture("mirror-full-platforms.yml", dir.path());
    match result {
        Ok(()) => {
            let workflow = dir.path().join(".github/workflows/mirror.yml");
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
            let workflow = dir.path().join(".github/workflows/mirror.yml");
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
            let workflow = dir.path().join(".github/workflows/mirror.yml");
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
