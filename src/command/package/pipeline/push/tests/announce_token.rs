// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

#[test]
fn a_blank_announce_token_reads_as_no_token_at_all() {
    // Both `push` and `patch` decide whether to announce on this one
    // predicate, and both degrade on `None`. A GitHub secret that is
    // configured-but-empty arrives as `""`, so treating "set" as "usable"
    // would send every such repository into an announce that can only 401.
    let _guard = job_url_env_lock();

    // SAFETY: test-only process env, serialised by the lock above.
    unsafe { std::env::set_var(ENV_ANNOUNCE_TOKEN, "gh-token") };
    assert_eq!(announce_token().as_deref(), Some("gh-token"));

    unsafe { std::env::set_var(ENV_ANNOUNCE_TOKEN, "   ") };
    assert_eq!(announce_token(), None, "a blank secret is not a credential");

    unsafe { std::env::remove_var(ENV_ANNOUNCE_TOKEN) };
    assert_eq!(announce_token(), None);
}
