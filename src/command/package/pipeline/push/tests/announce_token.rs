// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::pipeline::ocx_cli::announce::missing_credential_hint;
use crate::test_support::EnvRestore;

/// An `announce:` block for `bazelbuild/bazelisk` with the given extra keys
/// (`transport`, `forge`, `index_repo`, ...) appended verbatim.
fn config(keys: &[(&str, &str)]) -> AnnounceConfig {
    let extra: String = keys.iter().map(|(key, value)| format!("{key}: {value}\n")).collect();
    serde_yaml_ng::from_str(&format!("package: bazelbuild/bazelisk\n{extra}")).unwrap()
}

/// The three announce gates (`push`, `patch`, `cascade`) decide on this one
/// predicate and degrade on `false`, so it has to answer the way `ocx`'s own
/// ladder does. A GitHub secret that is configured-but-empty arrives as
/// `""`: treating "set" as "usable" would send every such repository into an
/// announce that can only 401. And a GitLab job carries no
/// `OCX_ANNOUNCE_TOKEN` at all — its credential is `CI_JOB_TOKEN`, which the
/// ladder admits only under `transport: git`, the one transport a job token
/// can open a merge request through.
#[test]
fn announce_credential_follows_the_forge_ladder_including_the_gitlab_job_token_rung() {
    let _guard = job_url_env_lock();
    let _restore = EnvRestore::set(&[
        (ENV_ANNOUNCE_TOKEN, Some("gh-token")),
        ("OCX_ANNOUNCE_GIT_TOKEN", None),
        ("GITLAB_CI", None),
        ("CI_JOB_TOKEN", None),
    ]);
    assert!(announce_credential_present(&config(&[])));

    // `""` is what a configured-but-empty GitHub secret arrives as. Judged
    // exactly as `ocx`'s ladder judges it — the gate mirrors the child, it
    // does not second-guess it — so no trimming here either.
    // SAFETY: test-only process env, serialised by the lock and restored by
    // the guard above.
    unsafe { std::env::set_var(ENV_ANNOUNCE_TOKEN, "") };
    assert!(
        !announce_credential_present(&config(&[])),
        "an empty secret is not a credential"
    );

    unsafe { std::env::remove_var(ENV_ANNOUNCE_TOKEN) };
    assert!(!announce_credential_present(&config(&[])));

    unsafe {
        std::env::set_var("GITLAB_CI", "true");
        std::env::set_var("CI_JOB_TOKEN", "glcbt-job");
    }
    assert!(
        announce_credential_present(&config(&[("transport", "git")])),
        "a GitLab job token is a credential under the git transport"
    );
    assert!(
        !announce_credential_present(&config(&[("transport", "api")])),
        "a job token cannot open a merge request through the API, so it is not one there"
    );
}

/// The skip notice's hint is per transport AND per forge, because the fix
/// is: a GitLab job under `transport: api` already HAS a token the ladder
/// refuses, and telling it to set `OCX_ANNOUNCE_TOKEN` alone hides the
/// one-line spec change that would let its job token through. On GitHub —
/// the default index — that same spec change is one `validate_announce_config`
/// refuses at exit 65 (`transport: git` is GitLab-only), so there the hint
/// must not suggest it.
#[test]
fn the_missing_credential_hint_names_the_fix_for_each_transport() {
    let plain = "set OCX_ANNOUNCE_TOKEN";
    let gitlab_api = "set OCX_ANNOUNCE_TOKEN — a GitLab CI_JOB_TOKEN cannot open a merge request over the api transport; \
                      use `transport: git` to announce with it";
    let git = "set OCX_ANNOUNCE_TOKEN, or run inside a GitLab job (GITLAB_CI + CI_JOB_TOKEN)";

    // GitHub: the default index, github.com spelled out, or an enterprise
    // host with the forge declared — no `transport: git` to offer.
    assert_eq!(
        missing_credential_hint(&config(&[])),
        plain,
        "the default index is on GitHub, where `transport: git` is refused"
    );
    assert_eq!(missing_credential_hint(&config(&[("transport", "api")])), plain);
    assert_eq!(
        missing_credential_hint(&config(&[("index_repo", "github.com/me/index")])),
        plain
    );
    assert_eq!(
        missing_credential_hint(&config(&[("index_repo", "ghe.example/me/index"), ("forge", "github")])),
        plain
    );

    // GitLab, inferred from the host or declared: the job-token clause.
    assert_eq!(
        missing_credential_hint(&config(&[("index_repo", "gitlab.com/me/index")])),
        gitlab_api
    );
    assert_eq!(missing_credential_hint(&config(&[("forge", "gitlab")])), gitlab_api);
    assert_eq!(
        missing_credential_hint(&config(&[
            ("index_repo", "git.example/me/index"),
            ("forge", "gitlab"),
            ("transport", "api")
        ])),
        gitlab_api
    );

    // A self-hosted index with no forge declared cannot be resolved (`plan`
    // already refused it): the plain hint, never a clause for a forge it
    // cannot name.
    assert_eq!(
        missing_credential_hint(&config(&[("index_repo", "git.example/me/index")])),
        plain
    );

    // `transport: git` is GitLab-only already, so the hint needs no forge.
    assert_eq!(missing_credential_hint(&config(&[("transport", "git")])), git);
    assert_eq!(
        missing_credential_hint(&config(&[("transport", "git"), ("forge", "gitlab")])),
        git
    );
}
