// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The dispatch-and-schedule workflows beside the publish pipeline:
//! `describe`, `announce-from-registry`, `patch`, `cascade`, and the
//! `verify-generated` drift guard.
//!
//! Each is a small render over its own template. They share the schedule and
//! concurrency helpers, which is why they sit together rather than beside the
//! publish workflow they orbit.

use super::matrix::ocx_cli_version;
use super::permissions::{render_discover_permissions, render_registry_auth_steps, render_registry_write_permissions};
use super::slot::{SpecSlot, indent_entries, slash_path, trigger_paths};
use super::{GIT_SHA_SHORT, VERSION};
use crate::spec::MirrorSpec;

pub const DESCRIBE_TEMPLATE: &str = include_str!("../templates/describe.yml");

pub const VERIFY_GENERATED_TEMPLATE: &str = include_str!("../templates/verify-generated.yml");

pub const ANNOUNCE_FROM_REGISTRY_TEMPLATE: &str = include_str!("../templates/announce-from-registry.yml");

pub const PATCH_TEMPLATE: &str = include_str!("../templates/patch.yml");

pub const CASCADE_TEMPLATE: &str = include_str!("../templates/cascade.yml");

/// Render the describe.yml catalog-publish workflow.
///
/// Lighter than `mirror.yml`: only the auth + target-registry placeholders need
/// substitution. The workflow itself triggers on changes to `CATALOG.md`,
/// `logo.*`, or `mirror.yml` and invokes
/// `ocx-mirror package pipeline describe` to publish the README + logo to the
/// `__ocx.desc` referrer tag on the target repository.
pub fn render_describe(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    let triggers = trigger_paths(
        slot,
        &["CATALOG.md".to_string(), "logo.*".to_string(), slot.source()],
        "describe",
    );

    DESCRIBE_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace("{TRIGGER_PATHS}", &triggers)
        .replace("{DESCRIBE_PERMISSIONS}", render_registry_write_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{TARGET_REGISTRY}", &spec.target.registry)
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// Render the `announce-from-registry.yml` catch-up workflow.
///
/// Same placeholder set as `describe.yml` — auth steps plus a GHCR permissions
/// block. Dispatch is always available and defaults to reporting: the push job
/// already announces what each run publishes, and this one exists for the
/// backlog a mirror that opted into `announce:` late can never reach by running
/// forward. `announce: { schedule: … }` adds a `schedule:` trigger whose runs
/// announce for real; a run that finds nothing new commits nothing, and opens a
/// pull request only for commits an earlier run stranded on the announce branch
/// (see `AnnounceReport` in `pipeline/push.rs`).
///
/// Keeps a concurrency group of its own rather than joining the push workflow's
/// the way `cascade.yml` does — see the template's comment on the group.
///
/// Takes **discover's** read-only permissions, not describe's. This job only
/// lists the target's tags and fetches their manifests — the writes it performs
/// all land on the index repository, through `OCX_ANNOUNCE_TOKEN`, never through
/// the job's own `GITHUB_TOKEN`. Handing it `packages: write` would grant the
/// one scope that could overwrite the very artifacts it exists to describe.
pub fn render_announce_from_registry(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    ANNOUNCE_FROM_REGISTRY_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace(
            "{ANNOUNCE_SCHEDULE_BLOCK}",
            &schedule_block(spec.announce.as_ref().and_then(|a| a.schedule.as_ref())),
        )
        .replace("{ANNOUNCE_PERMISSIONS}", render_discover_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// Render the `patch.yml` metadata-correction workflow.
///
/// Dispatch-only, and deliberately not wired to `pipeline plan`'s `has_drift`
/// output: a drift finding says the published metadata no longer matches the
/// spec, and whether that is worth re-emitting every affected manifest is a
/// maintainer's call. The point of generating it at all is that patching
/// otherwise needs registry push credentials and an index token on somebody's
/// laptop, which is not how any other pipeline verb is run.
///
/// The three `workflow_dispatch` inputs are the command's whole selection
/// surface; the run step turns each present one into its flag and each absent
/// one into nothing, so an empty dispatch patches every published version.
///
/// Takes the same `packages: write` block describe does — this job re-emits
/// manifests on the target repository. The announce it chains into writes to
/// the index repository with `OCX_ANNOUNCE_TOKEN`.
pub fn render_patch(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    PATCH_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace("{PATCH_PERMISSIONS}", render_registry_write_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// The workflow-level `concurrency:` group of *this spec's* push workflow.
///
/// `workflow.yml` spells it `mirror-${{ github.workflow }}-publish`, and
/// `github.workflow` is that workflow's `name:` — which the renderer sets to
/// `spec.name`. Resolving it here lets another workflow name the same group
/// without a runtime handle on the push workflow.
pub fn publish_concurrency_group(spec: &MirrorSpec) -> String {
    format!("mirror-{}-publish", spec.name)
}

/// A workflow's `schedule:` trigger, or nothing.
///
/// The templates place the placeholder on the line above `workflow_dispatch:`,
/// so an absent cron collapses to no lines at all.
///
/// The cron lands inside a single-quoted scalar unescaped; what keeps a spec
/// from closing it and appending triggers of its own is `spec::validate_cron`,
/// which every caller's spec passes through before any file is written.
pub fn schedule_block(cron: Option<&String>) -> String {
    cron.map(|cron| format!("  schedule:\n    - cron: '{}'\n", cron))
        .unwrap_or_default()
}

/// Render the `cascade.yml` rolling-tag repair workflow.
///
/// Dispatch is always available and defaults to `dry_run: true`, so a repair
/// that nobody asked for in writing only audits. `cascade: { schedule: … }`
/// adds a `schedule:` trigger whose runs repair for real. Emitted only for a
/// spec that cascades — a mirror publishing no rolling alias has no graph to
/// repair.
///
/// Shares the push workflow's concurrency group (see
/// [`publish_concurrency_group`]) so a repair never runs while a publish is
/// mid-way through writing the same aliases. GitHub holds one pending run per
/// group, so the trade is that a run *waiting* in that group — a repair or a
/// publish — is cancelled when a newer run of either workflow queues.
///
/// Takes the same `packages: write` block describe and patch do: a repair
/// writes tags to the target repository. The announce it chains into writes to
/// the index repository with `OCX_ANNOUNCE_TOKEN`.
pub fn render_cascade(spec: &MirrorSpec, slot: &SpecSlot) -> String {
    CASCADE_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &slot.source())
        .replace("{SPEC_ARG}", &slot.spec_arg())
        .replace("{WORKFLOW_SUFFIX}", &slot.suffix())
        .replace(
            "{CASCADE_SCHEDULE_BLOCK}",
            &schedule_block(spec.cascade.schedule.as_ref()),
        )
        .replace("{PUSH_CONCURRENCY_GROUP}", &publish_concurrency_group(spec))
        .replace("{CASCADE_PERMISSIONS}", render_registry_write_permissions(spec))
        .replace("{REGISTRY_AUTH_STEPS}", &render_registry_auth_steps(spec))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}

/// The `--spec` arguments the drift guard re-renders the repository with.
///
/// Empty for the lone repo-root `mirror.yml`, whose path is what `--spec`
/// already defaults to — that is what keeps every published mirror's committed
/// guard byte-identical. As soon as the repository holds a spec the default
/// cannot name, *every* spec is listed: `--spec` appends, so naming one would
/// silently drop the rest and the guard would check a subset while looking green.
pub fn verify_spec_args(slots: &[&SpecSlot]) -> String {
    if slots.iter().all(|slot| slot.is_default()) {
        return String::new();
    }
    slots.iter().map(|slot| format!(" --spec {}", slot.source())).collect()
}

/// Render the `verify-generated.yml` drift-guard workflow.
///
/// The workflow runs `ocx-mirror package pipeline generate ci --check` on pull requests
/// and pushes, so a hand-edit to any generated workflow fails CI. Exactly one is
/// emitted per repository — it names every spec, and its path triggers are the
/// union of theirs, which makes the committed file the record of what the
/// repository mirrors. Emitted unless *every* spec opts out via
/// `allow_manual_edits` (see [`render`]).
pub fn render_verify_generated(slots: &[&SpecSlot]) -> String {
    let mut entries = Vec::new();
    for slot in slots {
        match slot.dir() {
            None => entries.extend([
                slot.source(),
                "scripts/**".to_string(),
                "tests/**".to_string(),
                "metadata*.json".to_string(),
            ]),
            Some(dir) => entries.push(format!("{}/**", slash_path(dir))),
        }
        entries.extend(slot.extends_entries());
    }
    entries.push(".github/workflows/**".to_string());
    // Siblings share one base, so the same path arrives once per child. Keep the
    // first occurrence: order carries which spec brought an entry in.
    let mut seen = std::collections::HashSet::new();
    entries.retain(|entry| seen.insert(entry.clone()));

    let sources = slots.iter().map(|slot| slot.source()).collect::<Vec<_>>().join(", ");

    VERIFY_GENERATED_TEMPLATE
        .replace("{OCX_MIRROR_VERSION}", VERSION)
        .replace("{OCX_MIRROR_REV}", GIT_SHA_SHORT)
        .replace("{SPEC_SOURCE}", &sources)
        .replace("{SPEC_ARGS}", &verify_spec_args(slots))
        .replace("{TRIGGER_PATHS}", &indent_entries(&entries))
        .replace("{OCX_CLI_VERSION}", ocx_cli_version())
}
