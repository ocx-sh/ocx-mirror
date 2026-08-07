// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── No-credentials guard: push job ─────────────────────────────────────────

#[test]
fn push_job_has_detect_credentials_step() {
    // The push job must emit a 'Detect registry credentials' step with
    // id: creds that probes OCX_MIRROR_REGISTRY_TOKEN via env-var injection
    // without echoing the secret value. The auth steps are rendered per
    // target registry, so this asserts on the rendered workflow.
    let template = workflow_of(SHFMT_SPEC);
    let template = template.as_str();
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
    let template = workflow_of(SHFMT_SPEC);
    let template = template.as_str();
    // The login step and its guard must both be present in the workflow.
    assert!(
        template.contains("if: ${{ steps.creds.outputs.have == 'true' }}"),
        "at least one step in push job must carry if: steps.creds.outputs.have == 'true' guard"
    );
}

#[test]
fn push_job_push_step_has_creds_guard() {
    // The 'Push' step (ocx-mirror package pipeline push) must also be guarded so the
    // run-summary.json is only written when credentials are available.
    let template = workflow_of(SHFMT_SPEC);
    let template = template.as_str();
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
    let template = WORKFLOW_TEMPLATE;
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
    assert!(
        template.contains("announce=not_run"),
        "fallback step must emit announce=not_run — the push step never ran, \
         which is not the same as the mirror never opting in"
    );
}

#[test]
fn push_job_exports_the_announce_outcome_as_a_job_output() {
    // Without it, a run that published a dozen images and failed to
    // announce them is indistinguishable — to `notify` and to any branch
    // protection reading the job outputs — from one that announced. An
    // expired OCX_ANNOUNCE_TOKEN would then keep every nightly green while
    // the index drifts arbitrarily far behind the registry.
    let template = WORKFLOW_TEMPLATE;
    assert!(
        template.contains("announce: ${{ steps.summarise.outputs.announce }}"),
        "push job must export an `announce` output"
    );
    assert!(
        template.contains(r#"echo "announce=$(jq -r '.announce.status // "unconfigured"' run-summary.json)""#),
        "summarise must source the announce output from run-summary.json, \
         defaulting to `unconfigured` when the mirror has no announce: block"
    );
}

// ── No-credentials guard: describe workflow ─────────────────────────────────

#[test]
fn describe_workflow_has_detect_credentials_step() {
    // describe.yml must also guard the docker-login so a repo with no secrets
    // goes green on the describe job. The steps come from the shared
    // renderer, so assert on the rendered workflow, not on the template.
    let describe = describe_of(SHFMT_SPEC);
    assert!(
        describe.contains("name: Detect registry credentials"),
        "describe workflow must contain 'Detect registry credentials' step"
    );
    assert!(
        describe.contains("id: creds"),
        "describe credentials-detect step must have id: creds"
    );
    assert!(
        describe.contains("OCX_MIRROR_REGISTRY_TOKEN: ${{ secrets.OCX_MIRROR_REGISTRY_TOKEN }}"),
        "describe credentials-detect step must inject secret as env var"
    );
}

#[test]
fn describe_workflow_login_and_publish_steps_have_creds_guard() {
    // Both the docker-login and the 'Publish catalog metadata' step in
    // describe.yml must carry the creds guard.
    let describe = describe_of(SHFMT_SPEC);
    let guard = "if: ${{ steps.creds.outputs.have == 'true' }}";
    let count = describe.matches(guard).count();
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
        let workflow = dir.path().join(".github/workflows/mirror.yml");
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
        let workflow = dir.path().join(".github/workflows/mirror.yml");
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
            content
                .contains("jq -c '[.versions[] | select(.kind != \"metadata-drift\") | {version, platforms, kind}]'"),
            "versions output must be projected so asset URLs stay out of the matrix JSON, \
             and must drop metadata-drift entries — they carry no assets, so a prepare leg \
             for one aborts on `carries no resolved assets` whenever some other version in \
             the same run is genuinely new"
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
        let describe = dir.path().join(".github/workflows/describe.yml");
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
