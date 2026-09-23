// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror version` — a copy of ocx's `version` command and its
//! `VersionData` / `VerboseVersionData` rendering. JSON comes from the root
//! `--format json` / `--json` (ocx's `ocx_console::Format`), as in ocx; the
//! verbose `host:` row names os/arch only: ocx adds the libc family from its
//! state store, which the mirror does not keep.

use ocx_console::DataInterface;
use serde::Serialize;

use crate::build_info::{self, Provenance};
use crate::error::MirrorError;
use crate::pipeline::options::OutputFormat;

/// Print the ocx-mirror version and the build provenance baked into it.
#[derive(clap::Args)]
pub struct Version {
    /// Emit enriched build provenance - commit, dirty flag, build time,
    /// target, rustc, CI run URL. JSON output always includes the
    /// populated subset; this flag only affects plain text.
    #[arg(short, long)]
    verbose: bool,

    /// The root `--format` / `--json`, set by `Command::apply_format`.
    #[arg(skip)]
    pub(super) format: Option<OutputFormat>,
}

impl Version {
    pub fn execute(&self, printer: &DataInterface) -> Result<(), MirrorError> {
        let data = VersionData::enriched(build_info::version(), env!("CARGO_PKG_VERSION"));
        match self.format.unwrap_or(OutputFormat::Plain) {
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(&data).map_err(|e| {
                    MirrorError::ExecutionFailed(vec![format!("cannot render the version as JSON: {e}")])
                })?;
                println!("{json}");
            }
            OutputFormat::Plain if self.verbose => data.print_verbose(printer),
            OutputFormat::Plain => println!("{}", data.version),
        }
        Ok(())
    }
}

/// The `version` report.
///
/// Plain: the bare version token, so a script can parse stdout as one
/// semver. JSON: `version` always; `cargo_pkg_version` only when
/// `__OCX_BUILD_VERSION` overrode it; the provenance blocks flattened in,
/// each absent when its source was unavailable at build time.
#[derive(Serialize)]
struct VersionData {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cargo_pkg_version: Option<String>,
    #[serde(flatten)]
    provenance: Provenance,
}

impl VersionData {
    fn enriched(version: impl Into<String>, cargo_pkg_version: impl Into<String>) -> Self {
        let version = version.into();
        let cargo_pkg = cargo_pkg_version.into();
        let cargo_pkg_version = (cargo_pkg != version).then_some(cargo_pkg);
        Self {
            version,
            cargo_pkg_version,
            provenance: Provenance::current(),
        }
    }

    /// Multi-line labelled summary: header, `host:`, then a row per
    /// provenance block that was baked in (absent blocks print no row).
    fn print_verbose(&self, printer: &DataInterface) {
        for line in self.verbose_lines(printer) {
            println!("{line}");
        }
    }

    fn verbose_lines(&self, printer: &DataInterface) -> Vec<String> {
        let theme = printer.theme();
        let mut lines = Vec::new();

        let mut header = format!("{} {}", theme.label("ocx-mirror"), theme.tag(&self.version));
        let mut extras: Vec<String> = Vec::new();
        if let Some(cargo) = &self.cargo_pkg_version {
            extras.push(format!("cargo: {}", theme.tag(cargo)));
        }
        if let Some(channel) = self.provenance.channel {
            extras.push(format!("channel: {}", theme.tag(channel)));
        }
        if !extras.is_empty() {
            header.push_str(&format!(" ({})", extras.join(", ")));
        }
        lines.push(header);

        if let Some(platform) = ocx_oci::Platform::current() {
            lines.push(format!("{}    {}", theme.label("host:"), platform.segments().join("/")));
        }

        if let Some(commit) = &self.provenance.commit {
            let dirty_text = if commit.dirty { "dirty" } else { "clean" };
            let timestamp = commit
                .timestamp
                .as_deref()
                .map(|ts| theme.aside(format!(" - {ts}")))
                .unwrap_or_default();
            lines.push(format!(
                "{}   {} {}{}",
                theme.label("commit:"),
                theme.digest(&commit.short),
                theme.aside(format!("({dirty_text})")),
                timestamp,
            ));
        }

        if let Some(build) = &self.provenance.build {
            lines.push(format!(
                "{}    {} {}",
                theme.label("built:"),
                build.timestamp,
                theme.aside(format!("({})", build.profile)),
            ));
            lines.push(format!("{}   {}", theme.label("target:"), theme.tag(&build.target)));
            lines.push(format!("{}    {}", theme.label("rustc:"), theme.tag(&build.rustc)));
        }

        if let Some(ci) = &self.provenance.ci {
            lines.push(format!(
                "{}       {}",
                theme.label("ci:"),
                theme.aside(ci.run_url.clone())
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::VersionData;
    use ocx_console::{DataInterface, Printer};

    #[test]
    fn json_keeps_the_version_key() {
        let value = serde_json::to_value(VersionData::enriched("0.3.1", "0.3.1")).unwrap();
        assert_eq!(value.get("version").and_then(|v| v.as_str()), Some("0.3.1"));
    }

    #[test]
    fn cargo_pkg_version_is_suppressed_when_equal() {
        let value = serde_json::to_value(VersionData::enriched("0.3.1", "0.3.1")).unwrap();
        assert!(value.get("cargo_pkg_version").is_none());
    }

    #[test]
    fn cargo_pkg_version_surfaces_when_overridden() {
        let value = serde_json::to_value(VersionData::enriched("0.3.2-dev+20260528143045", "0.3.1")).unwrap();
        assert_eq!(value.get("cargo_pkg_version").and_then(|v| v.as_str()), Some("0.3.1"));
    }

    /// The JSON wire shape is the version plus the provenance blocks — a new
    /// top-level key is a contract change and must be added here on purpose.
    #[test]
    fn json_top_level_keys_are_the_documented_set() {
        let value = serde_json::to_value(VersionData::enriched("1.2.3", "1.0.0")).unwrap();
        for key in value.as_object().expect("wire shape must be a JSON object").keys() {
            assert!(
                ["version", "cargo_pkg_version", "channel", "commit", "build", "ci"].contains(&key.as_str()),
                "unexpected top-level key {key:?} in version JSON"
            );
        }
    }

    /// Verbose plain: the header names the version and every qualifier, one
    /// row per baked block, and no ANSI bytes with colour off.
    #[test]
    fn verbose_lines_render_every_baked_block_without_ansi() {
        let data = VersionData::enriched("1.2.3", "1.0.0");
        let lines = data.verbose_lines(&DataInterface::new(Printer::new(false, false)));
        let header = &lines[0];
        assert!(header.starts_with("ocx-mirror 1.2.3 (cargo: 1.0.0"), "{header}");
        if let Some(channel) = data.provenance.channel {
            assert!(header.contains(&format!("channel: {channel}")), "{header}");
        }
        let has_row = |label: &str| lines.iter().any(|line| line.starts_with(label));
        assert_eq!(has_row("commit:"), data.provenance.commit.is_some(), "{lines:?}");
        assert_eq!(has_row("built:"), data.provenance.build.is_some(), "{lines:?}");
        assert_eq!(has_row("ci:"), data.provenance.ci.is_some(), "{lines:?}");
        if let Some(commit) = &data.provenance.commit {
            assert!(lines.iter().any(|line| line.contains(&commit.short)), "{lines:?}");
        }
        assert!(lines.iter().all(|line| !line.contains('\x1b')), "{lines:?}");
    }
}
