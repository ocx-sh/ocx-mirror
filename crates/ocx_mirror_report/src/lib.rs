// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! What a pipeline run reports: the JUNIT files its test legs write
//! ([`junit`]), the `run-summary.json` it hands to `notify`
//! ([`run_summary`]), and the Discord webhook `notify` posts to ([`discord`]).
//!
//! Errors are local ([`junit::JunitError`], [`discord::WebhookError`]);
//! `ocx_mirror_error` converts each into the `MirrorError` variant it
//! replaced, with the same message and exit code.

pub mod discord;
pub mod junit;
pub mod run_summary;
