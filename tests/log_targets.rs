// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The documented debug recipe survives the crate split
//! (adr_bazel_crate_split.md § C2 "Log targets change", D7; plan C-012).
//!
//! `tracing` names an event's target after the module path that emits it, so
//! code moved into `crates/ocx_mirror_pipeline` logs under
//! `ocx_mirror_pipeline::…`, not `ocx_mirror::pipeline::…`. The recipe in
//! `docs/reference/cli.md` (`RUST_LOG=info,ocx_mirror=debug,…`) still reaches
//! every member because an `EnvFilter` directive matches a target by string
//! prefix. This test pins that: one debug event per member crate, all
//! captured under the recipe read from the doc itself, and a foreign crate's
//! debug event is not.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// The recipe `docs/reference/cli.md` documents: the value of its one
/// `` `RUST_LOG=…` `` code span. Read from the doc, not copied, so a doc edit
/// cannot drift from what this file proves.
fn recipe() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/reference/cli.md");
    let doc = std::fs::read_to_string(path).expect("docs/reference/cli.md is readable");
    let marker = "`RUST_LOG=";
    let start = doc
        .find(marker)
        .unwrap_or_else(|| panic!("docs/reference/cli.md no longer documents a {marker}…` recipe"))
        + marker.len();
    let len = doc[start..]
        .find('`')
        .expect("the `RUST_LOG=` code span in docs/reference/cli.md is closed");
    doc[start..start + len].to_owned()
}

/// One literal list feeds both the name list and the emitters: a callsite's
/// target must be `'static`, so each crate needs its own `debug!` invocation.
macro_rules! member_targets {
    ($($target:literal),* $(,)?) => {
        /// Every mirror crate the recipe must reach — `crates/crate_map.toml`
        /// `[allowed]` keys minus `ocx_python`, which is not a mirror crate
        /// and is outside the `ocx_mirror` prefix by design.
        const MEMBER_TARGETS: &[&str] = &[$($target),*];

        fn emit_member_events() {
            $(tracing::debug!(target: $target, "probe");)*
        }
    };
}

member_targets!(
    "ocx_mirror",
    "ocx_mirror_error",
    "ocx_mirror_http",
    "ocx_mirror_pipeline",
    "ocx_mirror_report",
    "ocx_mirror_source",
    "ocx_mirror_spec",
    "ocx_mirror_test_support",
);

/// Module-shaped targets as real callsites produce them: the root's own
/// `command/` code and a module inside a member crate.
const MODULE_TARGETS: [&str; 2] = ["ocx_mirror::command::x", "ocx_mirror_pipeline::registry_sync::x"];

/// A dependency's debug output, which the recipe keeps silent. Not `reqwest`:
/// the documented recipe raises that one to `trace` on purpose.
const CONTROL_TARGET: &str = "hyper";

/// Records the target of every event the filter lets through.
struct Capture(Arc<Mutex<Vec<String>>>);

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        // A bridged `log` record arrives under the fixed target `log`, its own
        // target in the `log.target` field (what `NormalizeEvent` reads).
        let mut log_target = LogTarget(None);
        event.record(&mut log_target);
        let target = log_target.0.unwrap_or_else(|| event.metadata().target().to_owned());
        self.0.lock().expect("capture lock").push(target);
    }
}

/// Reads a bridged record's `log.target` field.
struct LogTarget(Option<String>);

impl Visit for LogTarget {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "log.target" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

#[test]
fn the_debug_recipe_reaches_every_mirror_crate_and_nothing_else() {
    let recipe = recipe();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::new(&recipe))
        .with(Capture(Arc::clone(&captured)));

    tracing::subscriber::with_default(subscriber, || {
        emit_member_events();
        tracing::debug!(target: "ocx_mirror::command::x", "probe");
        tracing::debug!(target: "ocx_mirror_pipeline::registry_sync::x", "probe");
        tracing::debug!(target: "hyper", "probe");
    });

    let captured = captured.lock().expect("capture lock").clone();
    for target in MEMBER_TARGETS.iter().chain(MODULE_TARGETS.iter()) {
        assert!(
            captured.iter().any(|seen| seen == target),
            "`RUST_LOG={recipe}` must enable debug events targeted `{target}`; captured: {captured:?}"
        );
    }
    assert!(
        !captured.iter().any(|seen| seen == CONTROL_TARGET),
        "`RUST_LOG={recipe}` must not enable `{CONTROL_TARGET}` debug events; captured: {captured:?}"
    );
    assert_eq!(
        captured.len(),
        MEMBER_TARGETS.len() + MODULE_TARGETS.len(),
        "exactly the mirror events are captured"
    );
}

/// Half the mirror logs through `log::debug!` (e.g. `ocx_mirror_pipeline`'s
/// orchestrator), which reaches the subscriber only via the `LogTracer` bridge
/// (`tracing-subscriber`'s `tracing-log` feature). The recipe must hold across
/// that bridge too. `set_default` installs the bridge itself (once per process,
/// a no-op after), so this test needs no global setup and no other test in
/// this binary uses `log`.
#[test]
fn the_debug_recipe_reaches_mirror_log_records_through_the_bridge() {
    let recipe = recipe();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::new(&recipe))
        .with(Capture(Arc::clone(&captured)));

    {
        let _guard = subscriber.set_default();
        log::debug!(target: "ocx_mirror_pipeline::orchestrator", "probe");
        log::debug!(target: CONTROL_TARGET, "probe");
    }

    let captured = captured.lock().expect("capture lock").clone();
    assert_eq!(
        captured,
        ["ocx_mirror_pipeline::orchestrator"],
        "`RUST_LOG={recipe}` must carry a mirror `log::debug!` record and drop `{CONTROL_TARGET}`'s"
    );
}

#[test]
fn the_target_list_names_every_mirror_crate_in_the_crate_map() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/crates/crate_map.toml");
    let map: toml::Table = toml::from_str(&std::fs::read_to_string(path).expect("crate map is readable"))
        .expect("crates/crate_map.toml parses");
    let allowed = map
        .get("allowed")
        .and_then(toml::Value::as_table)
        .expect("crate map has an [allowed] table");

    let mut expected: Vec<&str> = allowed
        .keys()
        .map(String::as_str)
        .filter(|name| *name != "ocx_python")
        .collect();
    expected.sort_unstable();
    let mut listed = MEMBER_TARGETS.to_vec();
    listed.sort_unstable();

    assert_eq!(
        listed, expected,
        "MEMBER_TARGETS must list every crates/crate_map.toml [allowed] key except ocx_python — \
         a crate missing here is a crate the debug recipe is not proven to reach"
    );
}
