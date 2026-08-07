// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `pipeline push` test modules, split by concern.
//!
//! Each child reaches `push.rs`'s private items through `use super::super::*;`
//! and the cross-module helpers through `use super::support::*;`.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/announce.rs"]
mod announce;
#[path = "tests/announce_token.rs"]
mod announce_token;
#[path = "tests/argv.rs"]
mod argv;
#[path = "tests/cascade_backfill.rs"]
mod cascade_backfill;
#[path = "tests/env_push.rs"]
mod env_push;
#[path = "tests/helpers.rs"]
mod helpers;
#[path = "tests/job_url.rs"]
mod job_url;
#[path = "tests/latest_alias.rs"]
mod latest_alias;
#[path = "tests/libc_gating.rs"]
mod libc_gating;
#[path = "tests/ordering.rs"]
mod ordering;
#[path = "tests/push_driver.rs"]
mod push_driver;
#[path = "tests/retry.rs"]
mod retry;
#[path = "tests/verdict_alias.rs"]
mod verdict_alias;
