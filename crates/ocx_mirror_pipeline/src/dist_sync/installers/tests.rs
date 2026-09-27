// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::path::Path;

use super::*;
use ocx_mirror_spec::ValueSource;

/// The embedded configuration block as `www-setup/src/install.sh` spells it.
const BLOCK: &str = "\
__ocx_cfg_dist_url='@OCX_INSTALL_DIST_URL@'
__ocx_cfg_mirror_url='@OCX_INSTALL_MIRROR_URL@'
__ocx_cfg_ca_bundle='@OCX_INSTALL_CA_BUNDLE@'
__ocx_cfg_managed_config='@OCX_MANAGED_CONFIG@'
";

fn prepared(ca_bundle: Option<&str>, managed_config: Option<&str>) -> Prepared {
    Prepared {
        installers: Vec::new(),
        snapshots: None,
        latest: None,
        ca_bundle: ca_bundle.map(str::to_string),
        managed_config: managed_config.map(str::to_string),
    }
}

#[test]
fn only_the_placeholders_the_spec_sets_are_filled() {
    let patched = prepared(None, None).patch(Shell::Sh, BLOCK, "https://art.test/dist.json");

    assert!(patched.contains("__ocx_cfg_dist_url='https://art.test/dist.json'"));
    // An unreplaced placeholder is what makes the installer fall back to its
    // built-in default, so an unset key must stay exactly as upstream wrote it.
    assert!(patched.contains("'@OCX_INSTALL_CA_BUNDLE@'"));
    assert!(patched.contains("'@OCX_MANAGED_CONFIG@'"));
}

#[test]
fn the_mirror_url_placeholder_is_never_filled() {
    // The mirrored manifest already names the mirror in every row; setting the
    // mirror knob too would re-compose URLs a package registry cannot serve.
    let patched =
        prepared(Some("/etc/ca.pem"), Some("reg.test/cfg:v1")).patch(Shell::Sh, BLOCK, "https://art.test/dist.json");

    assert!(patched.contains("'@OCX_INSTALL_MIRROR_URL@'"));
}

#[test]
fn a_multi_line_pem_is_embedded_verbatim_inside_the_quotes() {
    let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
    let patched = prepared(Some(pem), None).patch(Shell::Sh, BLOCK, "https://art.test/dist.json");

    assert!(patched.contains(&format!("__ocx_cfg_ca_bundle='{pem}'")));
}

#[tokio::test]
async fn a_single_quote_in_a_site_value_is_refused() {
    let value = ValueSource::Literal("reg.test/it's:v1".to_string());

    let error = resolve_value(Some(&value), "publish.installers.managed_config", Path::new("."))
        .await
        .expect_err("a single quote must be refused");

    assert!(matches!(error, MirrorError::SpecInvalid(_)), "{error:?}");
}

#[tokio::test]
async fn a_file_value_is_read_relative_to_the_spec_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("ca.pem"), "-----BEGIN CERTIFICATE-----\nX\n").expect("write fixture");
    let value: ValueSource = serde_yaml_ng::from_str("file: ca.pem").expect("a file value must parse");

    let resolved = resolve_value(Some(&value), "publish.installers.ca_bundle", dir.path())
        .await
        .expect("the file must resolve");

    assert_eq!(resolved.as_deref(), Some("-----BEGIN CERTIFICATE-----\nX"));
}

#[test]
fn fish_gets_its_backslashes_doubled_and_no_other_dialect_does() {
    // fish reads `\\` and `\'` as escapes inside single quotes: a Windows path
    // ending in a backslash would otherwise swallow the closing quote.
    let value = prepared(Some(r"C:\certs\"), None);

    let fish = value.patch(Shell::Fish, BLOCK, "https://art.test/dist.json");
    let sh = value.patch(Shell::Sh, BLOCK, "https://art.test/dist.json");

    assert!(fish.contains(r"__ocx_cfg_ca_bundle='C:\\certs\\'"), "{fish}");
    assert!(sh.contains(r"__ocx_cfg_ca_bundle='C:\certs\'"), "{sh}");
}

#[tokio::test]
async fn a_typographic_quote_is_refused_like_a_plain_one() {
    // PowerShell closes a single-quoted string on U+2018–U+201B too.
    let value = ValueSource::Literal("reg.test/it\u{2019}s:v1".to_string());

    let error = resolve_value(Some(&value), "publish.installers.managed_config", Path::new("."))
        .await
        .expect_err("a typographic quote must be refused");

    assert!(matches!(error, MirrorError::SpecInvalid(_)), "{error:?}");
}
