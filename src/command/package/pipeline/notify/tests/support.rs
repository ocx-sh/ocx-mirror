// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `notify` test module.

use std::io::Write as _;

use super::super::*;
use crate::run_summary::{LayerReuse, PlatformFailure, RunSummary, TestFailure, VersionStatus, VersionSummary};
use tempfile::NamedTempFile;

pub fn col_platform(embed: &DiscordEmbed) -> &DiscordEmbedField {
    &embed.fields[0]
}

pub fn col_status(embed: &DiscordEmbed) -> &DiscordEmbedField {
    &embed.fields[1]
}

pub fn make_all_failed_summary() -> RunSummary {
    let failure = PlatformFailure {
        platform: "linux/amd64".to_string(),
        reason: "test_failed".to_string(),
        failed_tests: vec![TestFailure {
            version: "3.7.0".to_string(),
            platform: "linux/amd64".to_string(),
            container: "ubuntu_2404".to_string(),
            test: "version".to_string(),
            message: "binary not found".to_string(),
        }],
        job_url: None,
    };
    let mut v = version(VersionStatus::Failed, "3.7.0");
    v.platforms_failed = vec![failure];
    let mut summary = run_summary(vec![v], false, true);
    summary.run_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/4".to_string();
    summary
}

pub fn make_all_green_summary() -> RunSummary {
    let mut v = version(VersionStatus::Published, "3.7.0");
    v.platforms_pushed = vec!["linux/amd64".to_string(), "darwin/arm64".to_string()];
    v.cascade_tags_written = vec![
        "3.7.0".to_string(),
        "3.7".to_string(),
        "3".to_string(),
        "latest".to_string(),
    ];
    let mut summary = run_summary(vec![v], true, false);
    summary.run_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/2".to_string();
    summary
}

pub fn make_partial_summary() -> RunSummary {
    let failure = PlatformFailure {
        platform: "darwin/amd64".to_string(),
        reason: "test_failed".to_string(),
        failed_tests: vec![TestFailure {
            version: "3.7.0".to_string(),
            platform: "darwin/amd64".to_string(),
            container: "_native_".to_string(),
            test: "smoke".to_string(),
            message: "exit 1".to_string(),
        }],
        job_url: None,
    };
    let mut v = version(VersionStatus::Partial, "3.7.0");
    v.platforms_pushed = vec!["linux/amd64".to_string()];
    v.platforms_failed = vec![failure];
    v.cascade_tags_written = vec!["3.7.0".to_string()];
    let mut summary = run_summary(vec![v], true, true);
    summary.run_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/3".to_string();
    summary
}

pub fn only_embed(messages: &[DiscordWebhookPayload]) -> &DiscordEmbed {
    assert_eq!(messages.len(), 1, "expected a single message: {messages:?}");
    assert_eq!(messages[0].embeds.len(), 1, "expected a single embed");
    &messages[0].embeds[0]
}

pub fn run_summary(versions: Vec<VersionSummary>, any_new_green: bool, any_red: bool) -> RunSummary {
    RunSummary {
        schema_version: 1,
        mirror: "shfmt".to_string(),
        target: "ocx.sh/shfmt".to_string(),
        run_url: "https://github.com/ocx-sh/mirror-shfmt/actions/runs/1".to_string(),
        push_job_url: None,
        source_url: None,
        logo_url: None,
        versions,
        announce: None,
        any_red,
        any_new_green,
    }
}

pub fn version(status: VersionStatus, version: &str) -> VersionSummary {
    VersionSummary {
        version: version.to_string(),
        status,
        platforms_pushed: vec![],
        platforms_failed: vec![],
        cascade_tags_written: vec![],
        test_failures: vec![],
        platforms_excluded: vec![],
        layer_reuse: LayerReuse::default(),
    }
}

pub fn write_run_summary(summary: &RunSummary) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    let json = serde_json::to_string_pretty(summary).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    f
}

/// RAII guard: holds the env lock and sets `OCX_MIRROR_DISCORD_HOOK` to
/// `url` for its lifetime; clears both notify env vars on drop.
pub struct WebhookEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

pub fn run_notify_sync(summary: &RunSummary) -> Result<(), MirrorError> {
    let f = write_run_summary(summary);
    let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
    let cmd = Notify {
        run_summary: f.path().to_path_buf(),
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async { cmd.execute(&printer).await });
    let _ = f; // keep alive
    result
}

impl Drop for WebhookEnvGuard {
    fn drop(&mut self) {
        // SAFETY: lock still held until self is fully dropped.
        unsafe {
            std::env::remove_var(WEBHOOK_ENV_VAR);
            std::env::remove_var(USER_ID_ENV_VAR);
        }
    }
}

pub fn make_all_skipped_summary() -> RunSummary {
    run_summary(vec![version(VersionStatus::SkippedExisting, "3.7.0")], false, false)
}

impl WebhookEnvGuard {
    pub fn set(url: &str) -> Self {
        let lock = webhook_env_lock();
        // SAFETY: env mutation is serialised by the held lock.
        unsafe { std::env::set_var(WEBHOOK_ENV_VAR, url) }
        Self { _lock: lock }
    }
    pub fn unset() -> Self {
        let lock = webhook_env_lock();
        // SAFETY: env mutation is serialised by the held lock.
        unsafe {
            std::env::remove_var(WEBHOOK_ENV_VAR);
            std::env::remove_var(USER_ID_ENV_VAR);
        }
        Self { _lock: lock }
    }
}

/// Serialises every test that mutates the shared `OCX_MIRROR_DISCORD_HOOK`
/// / `OCX_MIRROR_DISCORD_USER_ID` process env vars.
pub fn webhook_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}
