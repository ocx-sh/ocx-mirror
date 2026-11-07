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
