// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use ocx_lib::package::bin_scan::ScanMode;
use serde::Deserialize;

/// `bin_scan:` — whether a mirror run derives the published `binaries` claim
/// from the extracted content tree instead of the spec's metadata file alone.
///
/// A 1:1 mapping of `ocx`'s [`ScanMode`], declared here because that type is
/// not `Deserialize` and the spec is the only place a mirror states the
/// choice. A plain boolean would collapse `verify` into `auto`, and `verify`
/// is the mode a mirror actually wants once it hand-lists `binaries`: it turns
/// the list into a regression test against upstream rearranging its archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinScanMode {
    /// Never scan (the default): `binaries` is exactly what the metadata file
    /// declares, or absent when it declares none.
    #[default]
    Off,
    /// Fill an absent `binaries` claim from the scan; pass a declared one
    /// through unverified.
    Auto,
    /// Fill an absent claim exactly as `auto`, and additionally check a
    /// declared one against the tree — an executable on the interface surface
    /// that the metadata file does not list, or a listed name present but not
    /// executable, fails the run.
    Verify,
}

impl BinScanMode {
    /// Whether this mode reads the extracted content tree.
    ///
    /// The one thing every download-free path has to ask: a scanned `binaries`
    /// claim cannot be recomputed without the tree, so `pipeline plan` and
    /// `pipeline patch` have to adopt the published one rather than compare
    /// against an expectation that structurally cannot carry it.
    pub fn scans(self) -> bool {
        self != Self::Off
    }
}

impl From<BinScanMode> for ScanMode {
    fn from(mode: BinScanMode) -> Self {
        match mode {
            BinScanMode::Off => ScanMode::Off,
            BinScanMode::Auto => ScanMode::Auto,
            BinScanMode::Verify => ScanMode::Verify,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire spellings are the spec grammar — a rename here is a breaking
    /// `mirror.yml` change, not a refactor.
    #[test]
    fn wire_spellings_map_onto_ocx_scan_modes() {
        for (yaml, mode, scan) in [
            ("off", BinScanMode::Off, ScanMode::Off),
            ("auto", BinScanMode::Auto, ScanMode::Auto),
            ("verify", BinScanMode::Verify, ScanMode::Verify),
        ] {
            let parsed: BinScanMode =
                serde_yaml_ng::from_str(yaml).unwrap_or_else(|e| panic!("'{yaml}' must parse as a bin_scan mode: {e}"));
            assert_eq!(parsed, mode, "'{yaml}' parsed as the wrong mode");
            assert_eq!(
                ScanMode::from(parsed),
                scan,
                "'{yaml}' maps onto the wrong ocx ScanMode"
            );
        }
    }

    /// Omitting the key must never start scanning: a mirror that hand-lists
    /// `binaries` would have the list silently verified, and one that lists
    /// none would start publishing a claim its publisher never made.
    #[test]
    fn the_default_is_off() {
        assert_eq!(BinScanMode::default(), BinScanMode::Off);
        assert!(!BinScanMode::default().scans());
        assert!(BinScanMode::Auto.scans());
        assert!(BinScanMode::Verify.scans());
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        serde_yaml_ng::from_str::<BinScanMode>("always").expect_err("only off/auto/verify are modes");
    }
}
