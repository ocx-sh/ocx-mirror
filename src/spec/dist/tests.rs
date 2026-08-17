// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::path::Path;

use super::*;

const SPEC_PATH: &str = "dist.yml";

/// Deserialize a `dist.yml` body with `kind:` already stripped, exactly as the
/// loader hands it to serde.
fn parse(yaml: &str) -> Result<DistSpec, serde_yaml_ng::Error> {
    serde_yaml_ng::from_str(yaml)
}

fn valid(yaml: &str) -> DistSpec {
    parse(yaml).expect("the fixture must deserialize")
}

const MINIMAL: &str = r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
";

#[test]
fn the_defaults_cover_a_spec_that_names_only_the_destination() {
    let spec = valid(MINIMAL);

    assert_eq!(spec.source.as_str(), "https://setup.ocx.sh/dist.json");
    assert_eq!(spec.publish.layout, "{tag}/{filename}");
    assert!(spec.select.min_version.is_none());
    assert!(spec.upload.is_none());
    assert!(spec.validate(Path::new(SPEC_PATH)).is_empty());
}

#[test]
fn the_default_retry_schedule_is_the_agreed_one() {
    let spec = valid(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload: {}
",
    );

    assert_eq!(
        spec.upload.expect("the fixture carries an upload block").retry_delays,
        DEFAULT_RETRY_DELAYS
    );
}

/// The array length *is* the retry count, so an empty array is the only way to
/// say 'do not retry' — and it must not silently fall back to the default.
#[test]
fn an_empty_retry_schedule_disables_retry() {
    let spec = valid(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload:
  retry_delays: []
",
    );

    assert!(
        spec.upload
            .expect("the fixture carries an upload block")
            .retry_delays
            .is_empty()
    );
}

#[test]
fn an_unknown_key_is_refused_rather_than_ignored() {
    let error = parse(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
selct:
  min_version: '1.0.0'
",
    )
    .expect_err("a typo must not be silently ignored");

    assert!(error.to_string().contains("selct"), "{error}");
}

#[test]
fn a_bearer_identity_needs_only_a_token_variable() {
    let spec = valid(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload:
  identity:
    type: bearer
    token_env: ART_TOKEN
",
    );

    assert!(matches!(
        spec.upload.and_then(|upload| upload.identity),
        Some(Identity::Bearer { token_env }) if token_env == "ART_TOKEN"
    ));
}

/// The tag makes the invalid combinations unrepresentable rather than a
/// validation rule someone forgets to write.
#[test]
fn a_token_variable_under_the_basic_variant_is_a_load_error() {
    let error = parse(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload:
  identity:
    type: basic
    username_env: ART_USER
    password_env: ART_PASSWORD
    token_env: ART_TOKEN
",
    )
    .expect_err("a bearer field under basic must be refused");

    assert!(error.to_string().contains("token_env"), "{error}");
}

#[test]
fn an_unknown_identity_type_is_refused() {
    assert!(
        parse(
            r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload:
  identity:
    type: kerberos
",
        )
        .is_err(),
        "only the implemented schemes may load"
    );
}

/// There is no literal variant, so a secret cannot reach a committed spec even
/// by accident. Pinned because adding one would be a one-line change that
/// looks like a convenience.
#[test]
fn a_literal_password_has_no_field_to_land_in() {
    assert!(
        parse(
            r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload:
  identity:
    type: basic
    username_env: ART_USER
    password: hunter2
",
        )
        .is_err(),
        "a literal password must not deserialize"
    );
}

#[test]
fn a_credential_in_the_source_url_is_refused() {
    let spec = valid(
        r"
output: ./public
source: https://ci:hunter2@setup.ocx.sh/dist.json
publish:
  base_url: https://art.test/ocx-dist
",
    );

    let errors = spec.validate(Path::new(SPEC_PATH));

    assert!(
        errors.iter().any(|error| error.starts_with("source:")),
        "userinfo in source must be refused: {errors:?}"
    );
    assert!(
        !errors.join(" ").contains("hunter2"),
        "the rejection must not echo the credential: {errors:?}"
    );
}

/// A credential in `base_url` would be copied into every mirrored manifest row
/// and served to every consumer.
#[test]
fn a_credential_in_the_publish_base_url_is_refused() {
    let spec = valid(
        r"
output: ./public
publish:
  base_url: https://ci:hunter2@art.test/ocx-dist
",
    );

    let errors = spec.validate(Path::new(SPEC_PATH));

    assert!(
        errors.iter().any(|error| error.starts_with("publish.base_url:")),
        "{errors:?}"
    );
}

#[test]
fn a_plaintext_url_is_refused_unless_its_host_is_trusted() {
    let spec = valid(
        r"
output: ./public
source: http://localhost:8000/dist.json
publish:
  base_url: https://art.test/ocx-dist
",
    );

    assert!(
        spec.validate(Path::new(SPEC_PATH))
            .iter()
            .any(|error| error.contains("plaintext")),
        "http must be refused by default"
    );
}

#[test]
fn a_trusted_host_may_be_reached_over_plaintext() {
    let spec = valid(
        r"
output: ./public
source: http://127.0.0.1:8000/dist.json
publish:
  base_url: http://127.0.0.1:8000/store
trusted_hosts:
  - 127.0.0.1
",
    );

    assert!(
        spec.validate(Path::new(SPEC_PATH)).is_empty(),
        "the acceptance harness needs this escape: {:?}",
        spec.validate(Path::new(SPEC_PATH))
    );
}

#[test]
fn a_non_http_scheme_is_refused() {
    let spec = valid(
        r"
output: ./public
source: file:///etc/dist.json
publish:
  base_url: https://art.test/ocx-dist
",
    );

    assert!(
        spec.validate(Path::new(SPEC_PATH))
            .iter()
            .any(|error| error.contains("not a supported scheme")),
        "a file:// manifest would read the mirror host's own filesystem"
    );
}

#[test]
fn an_unknown_layout_placeholder_is_a_validation_error() {
    let spec = valid(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
  layout: '{tag}/{sha256}'
",
    );

    assert!(
        spec.validate(Path::new(SPEC_PATH))
            .iter()
            .any(|error| error.starts_with("publish.layout:")),
        "an unknown placeholder must fail at load, not half way through a copy"
    );
}

/// Two consumers compose onto `publish.base_url` and drop a query
/// differently, so a base carrying one advertises a byte at a URL it was not
/// stored at — and an Azure SAS, which the documentation recommends, would put
/// a live write credential into every published manifest row.
#[test]
fn a_publish_base_url_with_a_query_or_fragment_is_refused() {
    for base in [
        "https://store.test/ocx?sv=2024-01-01&sig=SECRET",
        "https://store.test/ocx#fragment",
    ] {
        let spec = valid(&format!(
            "
output: ./public
publish:
  base_url: {base}
"
        ));

        let errors = spec.validate(Path::new(SPEC_PATH));

        assert!(
            errors.iter().any(|error| error.starts_with("publish.base_url:")),
            "{base} must be refused: {errors:?}"
        );
    }
}

/// `Authorization` belongs in `identity:`, whose values come from the
/// environment. Allowing it here would reopen the literal-secret hole the
/// tagged enum closes.
#[test]
fn an_authorization_header_is_refused() {
    let spec = valid(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload:
  headers:
    Authorization: Bearer hunter2
",
    );

    let errors = spec.validate(Path::new(SPEC_PATH));

    assert!(
        errors.iter().any(|error| error.starts_with("upload.headers:")),
        "{errors:?}"
    );
}

#[test]
fn a_vendor_header_is_accepted() {
    let spec = valid(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
upload:
  headers:
    x-ms-blob-type: BlockBlob
",
    );

    assert!(spec.validate(Path::new(SPEC_PATH)).is_empty());
}

#[test]
fn an_empty_output_is_refused() {
    let spec = valid(
        r"
output: ''
publish:
  base_url: https://art.test/ocx-dist
",
    );

    assert!(
        spec.validate(Path::new(SPEC_PATH))
            .iter()
            .any(|error| error.starts_with("output:")),
    );
}

/// The tagged enum is what keeps invalid credential combinations out of
/// editors: `schema dist` must render `identity` as a `oneOf` whose branches
/// carry different fields.
#[cfg(feature = "jsonschema")]
#[test]
fn the_generated_schema_renders_identity_as_a_tagged_choice() {
    let schema = schemars::schema_for!(DistSpec);
    let rendered = serde_json::to_string(&schema).expect("the schema must serialize");

    assert!(rendered.contains("token_env"), "the bearer branch must be described");
    assert!(rendered.contains("password_env"), "the basic branch must be described");
    assert!(
        rendered.contains("oneOf") || rendered.contains("anyOf"),
        "an internally tagged enum must render as a choice: {rendered}"
    );
}

/// Every rule reports rather than short-circuiting, so an operator fixes one
/// spec once.
#[test]
fn every_violation_is_reported_in_one_pass() {
    let spec = valid(
        r"
output: ''
source: ftp://example.test/dist.json
publish:
  base_url: https://art.test/ocx-dist
  layout: '{nope}'
",
    );

    assert_eq!(
        spec.validate(Path::new(SPEC_PATH)).len(),
        3,
        "output, source scheme and layout are three independent violations"
    );
}
