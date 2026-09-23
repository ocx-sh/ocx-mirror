// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Compile-time-baked build provenance — a copy of ocx's `app::build_info`
//! plus its `app::version`.
//!
//! [`Provenance::current`] returns the build metadata embedded into the
//! binary by `build.rs`: git commit + dirty flag, build timestamp + profile +
//! target + rustc version, GitHub Actions run URL (when built in CI), and
//! release channel. Every field is [`Option`] because a tarball checkout
//! without `.git/` or a local `cargo build` outside CI cannot populate the
//! missing piece, and an absent field must omit cleanly from
//! `ocx-mirror --json version`.
//!
//! Every value here is `option_env!()` — resolved at compile time. None of
//! these accessors read the runtime environment.
//!
//! A `__testing` build and every Bazel build carry the fixed placeholders of
//! `testing_provenance.env` instead of real values (see `build.rs`).

use serde::Serialize;

/// Length of the abbreviated git SHA shown to humans (8 hex chars).
const SHORT_SHA_LEN: usize = 8;

/// Effective ocx-mirror version embedded in the binary.
///
/// `__OCX_BUILD_VERSION` when the build set it (`build-matrix.yml` forwards
/// the dev-deploy / release SemVer), else the `Cargo.toml` version.
pub fn version() -> &'static str {
    option_env!("__OCX_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The 8-character abbreviated commit SHA, when the build baked one.
pub fn short_sha() -> Option<&'static str> {
    let sha = option_env!("VERGEN_GIT_SHA")?;
    Some(sha.get(..SHORT_SHA_LEN).unwrap_or(sha))
}

/// Full build provenance for the running binary.
///
/// JSON shape (all sub-objects optional; absent in JSON when source data
/// was unavailable at build time):
///
/// ```json
/// {
///   "channel": "dev",
///   "commit": { "sha": "…", "short": "…", "describe": "…",
///               "dirty": false, "timestamp": "…" },
///   "build":  { "timestamp": "…", "profile": "release",
///               "target":    "…", "rustc":   "…" },
///   "ci":     { "provider":   "github-actions",
///               "run_url":    "…", "workflow": "…",
///               "ref":        "…", "sha":      "…" }
/// }
/// ```
#[derive(Serialize)]
pub struct Provenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiInfo>,
}

/// Git commit metadata baked at build time.
#[derive(Serialize)]
pub struct CommitInfo {
    /// Full 40-character SHA-1.
    pub sha: String,
    /// 8-character abbreviated SHA — convenience for humans.
    pub short: String,
    /// `git describe --tags --dirty` output, including any `-dirty` suffix.
    pub describe: String,
    /// `true` if the working tree had uncommitted changes when built.
    pub dirty: bool,
    /// ISO-8601 commit timestamp (author date).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// Build environment metadata (timestamp, profile, target, rustc).
#[derive(Serialize)]
pub struct BuildInfo {
    /// ISO-8601 UTC build timestamp.
    pub timestamp: String,
    /// `"release"` or `"debug"` — derived from the `CARGO_DEBUG` flag.
    pub profile: &'static str,
    /// Target triple the binary was compiled for.
    pub target: String,
    /// `rustc` version that compiled the binary.
    pub rustc: String,
}

/// GitHub Actions context. Present only when built under a GitHub
/// workflow that exported the standard `GITHUB_*` env vars.
#[derive(Serialize)]
pub struct CiInfo {
    pub provider: &'static str,
    /// Direct link to the run that produced this binary.
    /// Composed as `{server_url}/{repository}/actions/runs/{run_id}`.
    pub run_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

impl Provenance {
    /// Build the [`Provenance`] for the running binary by reading the
    /// compile-time-baked env vars.
    pub fn current() -> Self {
        Self {
            channel: option_env!("__OCX_BUILD_CHANNEL"),
            commit: commit_info(),
            build: build_info(),
            ci: ci_info(),
        }
    }
}

fn commit_info() -> Option<CommitInfo> {
    let sha = option_env!("VERGEN_GIT_SHA")?;
    // Empty on a checkout without tags (CI's default shallow clone): fall
    // back to the SHA rather than report an empty describe.
    let describe = option_env!("VERGEN_GIT_DESCRIBE")
        .filter(|describe| !describe.is_empty())
        .unwrap_or(sha);
    let dirty = matches!(option_env!("VERGEN_GIT_DIRTY"), Some("true"));
    Some(CommitInfo {
        sha: sha.to_owned(),
        short: short_sha().unwrap_or(sha).to_owned(),
        describe: describe.to_owned(),
        dirty,
        timestamp: option_env!("VERGEN_GIT_COMMIT_TIMESTAMP").map(str::to_owned),
    })
}

fn build_info() -> Option<BuildInfo> {
    let timestamp = option_env!("VERGEN_BUILD_TIMESTAMP")?.to_owned();
    let target = option_env!("VERGEN_CARGO_TARGET_TRIPLE")?.to_owned();
    let rustc = option_env!("VERGEN_RUSTC_SEMVER")?.to_owned();
    let profile = match option_env!("VERGEN_CARGO_DEBUG") {
        Some("true") => "debug",
        _ => "release",
    };
    Some(BuildInfo {
        timestamp,
        profile,
        target,
        rustc,
    })
}

fn ci_info() -> Option<CiInfo> {
    let server_url = option_env!("GITHUB_SERVER_URL")?;
    let repository = option_env!("GITHUB_REPOSITORY")?;
    let run_id = option_env!("GITHUB_RUN_ID")?;
    let run_url = format!("{server_url}/{repository}/actions/runs/{run_id}");
    Some(CiInfo {
        provider: "github-actions",
        run_url,
        workflow: option_env!("GITHUB_WORKFLOW").map(str::to_owned),
        git_ref: option_env!("GITHUB_REF").map(str::to_owned),
        sha: option_env!("GITHUB_SHA").map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test build (`__testing` under cargo, every Bazel build) reports the
    /// placeholders byte-for-byte, so the acceptance binary is the same across
    /// commits. `channel == "test"` is the marker; any other build only gets
    /// the structural checks below — `option_env!` is fixed at compile time,
    /// so which arm runs depends on how this test binary was built.
    #[test]
    fn current_is_the_placeholder_set_in_a_test_build_and_well_formed_otherwise() {
        let info = Provenance::current();
        if info.channel == Some("test") {
            let value = serde_json::to_value(&info).unwrap();
            assert_eq!(
                value["commit"],
                serde_json::json!({
                    "sha": "0000000000000000000000000000000000000000",
                    "short": "00000000",
                    "describe": "placeholder-g00000000",
                    "dirty": true,
                    "timestamp": "1970-01-01T00:00:00.000000000Z",
                })
            );
            assert_eq!(
                value["ci"],
                serde_json::json!({
                    "provider": "github-actions",
                    "run_url": "https://ci.invalid/placeholder/placeholder/actions/runs/0",
                    "workflow": "placeholder",
                    "ref": "refs/heads/placeholder",
                    "sha": "0000000000000000000000000000000000000000",
                })
            );
            if let Some(build) = &info.build {
                assert_eq!(build.timestamp, "1970-01-01T00:00:00.000000000Z");
            }
            assert_eq!(short_sha(), Some("00000000"));
        }
        if let Some(commit) = &info.commit {
            assert_eq!(commit.sha.len(), 40, "full SHA-1 expected: {}", commit.sha);
            assert_eq!(commit.short, &commit.sha[..SHORT_SHA_LEN]);
            assert_eq!(short_sha(), Some(commit.short.as_str()));
        } else {
            assert_eq!(short_sha(), None);
        }
        if let Some(build) = &info.build {
            assert!(!build.target.is_empty(), "target triple must be non-empty");
            assert!(!build.rustc.is_empty(), "rustc semver must be non-empty");
        }
        if let Some(ci) = &info.ci {
            assert!(!ci.run_url.is_empty(), "CI run URL must be non-empty");
        }
        if let Some(channel) = info.channel {
            assert!(!channel.is_empty(), "channel must be non-empty when present");
        }
    }

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty(), "effective version must not be empty");
    }

    /// JSON output skips empty blocks rather than emitting `null`.
    #[test]
    fn empty_provenance_serializes_to_empty_object() {
        let info = Provenance {
            channel: None,
            commit: None,
            build: None,
            ci: None,
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    /// `git_ref` in the struct, `"ref"` in JSON (a Rust keyword).
    #[test]
    fn ci_info_renames_ref_field() {
        let ci = CiInfo {
            provider: "github-actions",
            run_url: "https://example/run".into(),
            workflow: None,
            git_ref: Some("refs/heads/main".into()),
            sha: None,
        };
        let value = serde_json::to_value(&ci).unwrap();
        assert_eq!(value.get("ref").and_then(|v| v.as_str()), Some("refs/heads/main"));
        assert!(value.get("git_ref").is_none(), "Rust field name must not leak");
    }
}
