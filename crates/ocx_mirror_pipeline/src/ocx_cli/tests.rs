// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::*;

/// A parent's `--offline`, `--remote`, `--config` and `--index` travel as
/// `__OCX_ENV_STATE`, not as these variables, so forwarding them would only
/// re-export what the operator typed into a child that already inherits it.
#[test]
fn the_forwarded_names_exclude_every_flag_mirrored_variable() {
    for dead in ["OCX_OFFLINE", "OCX_REMOTE", "OCX_CONFIG", "OCX_INDEX"] {
        assert!(!FORWARDED_OCX_VARS.contains(&dead), "{dead} is no longer forwarded");
    }
}

/// Every forwarded name is one the ocx registry still declares.
#[test]
fn every_forwarded_name_is_declared_by_the_ocx_registry() {
    for name in FORWARDED_OCX_VARS {
        assert!(
            ocx_env::all().any(|var| var.name == *name),
            "{name} is not declared in ocx_env"
        );
    }
}

/// The mirror never clears a child's environment, so the execution state a
/// parent ocx handed down reaches the mirror's ocx children by inheritance.
#[cfg(unix)]
#[tokio::test]
async fn a_spawned_child_inherits_the_execution_state() {
    let state_key = ocx_env::__OCX_ENV_STATE.name;
    let _lock = ocx_mirror_test_support::OCX_ENV_LOCK.lock().await;
    let _env = ocx_mirror_test_support::EnvRestore::set(&[(state_key, Some("ocx-state-sentinel"))]);

    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", "printf %s \"$__OCX_ENV_STATE\""]);
    forward_ocx_env(&mut cmd);
    let output = cmd.output().await.expect("spawn sh");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ocx-state-sentinel");
}

/// A failing sign child's message reaches the operator from whichever stream
/// carried it.
///
/// Under `--format json` ocx reports a failure in its stdout envelope, so a
/// sweep whose every tag failed arrives with an empty stderr — and logging
/// stderr alone left `ERROR signing … failed with exit code 79` as the entire
/// account of what went wrong.
#[test]
fn a_failing_sign_child_is_reported_from_whichever_stream_spoke() {
    let envelope = r#"{"exit_code":79,"data":{"tags":[{"tag":"3.8.0","status":"failed"}]}}"#;

    assert_eq!(
        failure_detail("", envelope),
        envelope,
        "stdout is used when stderr is silent"
    );
    assert_eq!(
        failure_detail("  \n ", envelope),
        envelope,
        "whitespace-only stderr is silent"
    );
    assert_eq!(
        failure_detail("no ambient credential provider\n", envelope),
        "no ambient credential provider",
        "stderr still wins when it has something to say",
    );
    assert_eq!(failure_detail("", ""), "", "both silent is not a panic");
}

/// An ocx >= 0.6.4 error document in stdout adds its kind and detail slug; with
/// silent stderr its message stands in for the text.
#[test]
fn an_error_document_adds_kind_and_detail() {
    let doc = r#"{"schema_version":1,"command":"package push","exit_code":80,"error":{"kind":"auth_error","detail":"oidc_token_rejected","message":"Fulcio rejected token","context":{}}}"#;

    assert_eq!(
        failure_detail("", doc),
        "Fulcio rejected token [auth_error: oidc_token_rejected]"
    );
    assert_eq!(
        failure_detail("boom\n", doc),
        "boom [auth_error: oidc_token_rejected]",
        "stderr text is kept"
    );
}

/// A document without `detail` still names its kind; one without a message and
/// with silent stderr is just the tag.
#[test]
fn an_error_document_without_detail_names_the_kind() {
    let doc = r#"{"error":{"kind":"not_found"}}"#;
    assert_eq!(failure_detail("", doc), "[not_found]");
    assert_eq!(failure_detail("gone", doc), "gone [not_found]");
}
