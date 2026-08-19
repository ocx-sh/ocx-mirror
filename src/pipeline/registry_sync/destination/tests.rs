// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Destination tests (C-011…C-015).
//!
//! The subject is a trust boundary: every catalog key here is **foreign data**
//! off an upstream index, so the refusal tables carry as much weight as the
//! accept cases. Nothing in this module asserts that a bad key was *repaired* —
//! a normalising mirror is how two distinct upstream names collide into one
//! destination.

use super::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn target(repository: &str) -> Target {
    Target {
        registry: "ghcr.io".into(),
        repository: repository.into(),
    }
}

/// Drive a catalog key through the whole destination pipeline exactly as the
/// plan phase will: parse, expand, then compose and validate.
///
/// Returns the rendered message on refusal so a table can assert on it without
/// caring which of the two stages did the refusing — which stage is an
/// implementation detail, "it was refused" is the contract.
fn destination(template: &str, registry_as: &str, prefix: &str, catalog_key: &str) -> Result<String, String> {
    let template = DestinationTemplate::parse(template).map_err(|error| error.to_string())?;
    let expanded = template
        .expand(registry_as, catalog_key, None)
        .map_err(|error| error.to_string())?;
    physical_repository(&target(prefix), &expanded).map_err(|error| error.to_string())
}

// ── C-011 — template parsing ────────────────────────────────────────────────

#[test]
fn parse_accepts_the_three_placeholders_and_the_literals_between_them() {
    let template = DestinationTemplate::parse("{registry}/{namespace}/{package}").expect("the canonical template");

    assert_eq!(
        template
            .expand("ocx.sh", "kitware/cmake", None)
            .expect("a well-formed key"),
        "ocx.sh/kitware/cmake"
    );
    assert!(template.uses_registry());
}

#[test]
fn parse_refuses_an_unknown_placeholder_and_names_it() {
    let Err(error) = DestinationTemplate::parse("{tenant}/{package}") else {
        panic!("`{{tenant}}` is not a known placeholder and must not parse");
    };

    assert_eq!(
        error,
        TemplateError::UnknownPlaceholder {
            name: "tenant".to_string()
        }
    );
    assert!(
        error.to_string().contains("tenant"),
        "the message must name the offending placeholder: {error}"
    );
}

#[test]
fn parse_refuses_a_placeholder_that_never_closes() {
    for template in ["mirror/{namespace", "{", "{registry}/{package"] {
        let Err(error) = DestinationTemplate::parse(template) else {
            panic!("`{template}` opens a placeholder it never closes and must not parse");
        };
        assert_eq!(
            error,
            TemplateError::UnterminatedPlaceholder {
                template: template.to_string()
            }
        );
    }
}

/// A near-miss spelling is an error, not an empty substitution. The closed set
/// is the whole reason the placeholder grammar is hand-rolled: a template
/// engine's default is to render an unknown name as nothing, which would
/// silently collapse every package onto one destination.
#[test]
fn parse_refuses_near_miss_placeholder_spellings() {
    for template in [
        "{Registry}/{package}",
        "{ registry }/{package}",
        "{registries}/{package}",
        "{}/{package}",
    ] {
        assert!(
            DestinationTemplate::parse(template).is_err(),
            "`{template}` does not name a known placeholder and must be refused"
        );
    }
}

#[test]
fn uses_registry_is_false_when_the_placeholder_is_absent() {
    for template in ["{namespace}/{package}", "mirror/fixed", ""] {
        assert!(
            !DestinationTemplate::parse(template)
                .expect("a template with no unknown placeholder")
                .uses_registry(),
            "`{template}` carries no {{registry}}"
        );
    }
}

// ── C-011 — the upstream placeholders ───────────────────────────────────────

/// `{upstream_host}`/`{upstream_repository}` substitute the reference the
/// package's root points at, not its catalog key.
///
/// This is the shape that makes a preserved-pointer mirror reachable: ocx's
/// `[mirrors]` map asks the mirror for `<path_prefix>/<upstream repository>`,
/// so the landing path has to end in exactly that.
#[test]
fn the_upstream_placeholders_substitute_the_reference_the_root_points_at() {
    let template =
        DestinationTemplate::parse("{upstream_host}/{upstream_repository}").expect("the upstream-keyed template");

    let expanded = template
        .expand(
            "ocx.sh",
            "charmbracelet/gum",
            Some(Upstream {
                host: "ghcr.io",
                repository: "ocx-contrib/charmbracelet/gum",
            }),
        )
        .expect("a well-formed key");

    assert_eq!(expanded, "ghcr.io/ocx-contrib/charmbracelet/gum");
    assert!(template.needs_upstream(), "the template cannot expand at plan time");
    assert!(template.uses_upstream_repository());
}

/// `needs_upstream` covers `{upstream_host}` alone, but
/// `uses_upstream_repository` does not — the two predicates answer different
/// questions and the host-only template is where they diverge.
#[test]
fn the_host_alone_defers_expansion_without_keying_the_destination_upstream() {
    let template = DestinationTemplate::parse("{upstream_host}/{namespace}/{package}").expect("a host-keyed template");

    assert!(
        template.needs_upstream(),
        "the host is only known once the root is read"
    );
    assert!(
        !template.uses_upstream_repository(),
        "the path still ends in the catalog key, so two sources can still collide"
    );
}

/// Expanding an upstream template without an [`Upstream`] is refused, never
/// silently rendered as an empty segment.
///
/// The plan phase gates on `needs_upstream`, so this is the guard that keeps
/// that gate from being load-bearing: a caller that forgets it gets an error
/// naming the template, not a destination missing a path segment.
#[test]
fn expanding_an_upstream_template_without_the_reference_is_refused() {
    let error = DestinationTemplate::parse("{upstream_repository}")
        .expect("the template parses")
        .expand("ocx.sh", "kitware/cmake", None)
        .expect_err("no upstream was supplied");

    assert!(
        matches!(error, TemplateError::UpstreamUnavailable { .. }),
        "the refusal must name the cause: {error:?}"
    );
}

/// An upstream host carrying a port is refused by the OCI grammar rather than
/// slugged into one.
///
/// The composition is what refuses it, so the message an operator sees names
/// the whole composed path. Documented behaviour, not an oversight: a `:` →
/// `_` mapping would be an identity every consumer of the mirror would then
/// have to know.
#[test]
fn an_upstream_host_with_a_port_is_refused_rather_than_slugged() {
    let expanded = DestinationTemplate::parse("{upstream_host}/{upstream_repository}")
        .expect("the template parses")
        .expand(
            "ocx.sh",
            "kitware/cmake",
            Some(Upstream {
                host: "registry.internal:5000",
                repository: "kitware/cmake",
            }),
        )
        .expect("expansion itself substitutes verbatim");

    assert!(
        physical_repository(&target("mirror"), &expanded).is_err(),
        "a port is not a legal OCI path component"
    );
}

// ── C-012 — expansion ───────────────────────────────────────────────────────

#[test]
fn expand_substitutes_plainly_and_keeps_literal_text_verbatim() {
    let template =
        DestinationTemplate::parse("tools/{registry}-{namespace}/{package}").expect("a literal-rich template");

    assert_eq!(
        template
            .expand("ocx.sh", "kitware/cmake", None)
            .expect("a well-formed key"),
        "tools/ocx.sh-kitware/cmake"
    );
}

#[test]
fn expand_refuses_a_key_that_is_not_namespace_slash_package() {
    for (key, shape) in [
        ("cmake", "a single segment"),
        ("", "an empty key"),
        ("/", "two empty segments"),
        ("/cmake", "an empty namespace"),
        ("kitware/", "an empty package"),
        ("a/b/c", "three segments"),
        ("kitware//cmake", "an empty interior segment"),
        ("foo/../../prod-images", "a traversal walking out of the prefix"),
    ] {
        let Err(error) = DestinationTemplate::parse("{namespace}/{package}")
            .expect("the template parses")
            .expand("ocx.sh", key, None)
        else {
            panic!("`{key}` ({shape}) is not '<namespace>/<package>' and must be refused");
        };
        assert_eq!(
            error,
            TemplateError::MalformedCatalogKey { key: key.to_string() },
            "`{key}` must be refused as a malformed key, not for some other reason"
        );
    }
}

/// The key's shape is foreign input regardless of whether this template happens
/// to substitute it. A template ignoring both key placeholders would otherwise
/// map every malformed key onto one destination and report nothing.
#[test]
fn expand_checks_the_key_even_when_the_template_ignores_it() {
    let template = DestinationTemplate::parse("mirror/{registry}").expect("a key-free template");

    assert!(
        template.expand("ocx.sh", "a/b/c", None).is_err(),
        "a malformed key must be refused even when nothing substitutes it"
    );
    assert_eq!(
        template
            .expand("ocx.sh", "kitware/cmake", None)
            .expect("a well-formed key"),
        "mirror/ocx.sh"
    );
}

/// Expansion is substitution, not sanitisation: it hands the composed value on
/// exactly as the upstream spelled it, and [`physical_repository`] is what
/// refuses it. Pinned because a "helpful" lowercase or path-clean here is the
/// exact failure this boundary exists to prevent.
#[test]
fn expand_repairs_nothing_it_substitutes() {
    let template = DestinationTemplate::parse("{namespace}/{package}").expect("the template parses");

    assert_eq!(
        template
            .expand("ocx.sh", "Foo/Bar", None)
            .expect("expansion does not judge charset"),
        "Foo/Bar",
        "uppercase must survive expansion verbatim so the grammar guard can refuse it"
    );
    assert!(
        physical_repository(&target("mirror"), "Foo/Bar").is_err(),
        "and the grammar guard must then refuse it"
    );
}

// ── C-013 — the grammar guard and prefix containment ────────────────────────

/// The headline property of this module: **no catalog key, whatever its shape,
/// can place content outside the configured `target.repository` prefix.**
///
/// Every row is a distinct escape shape an upstream index could publish. The
/// assertion is refusal, never repair — a row that started passing because the
/// value was normalised would be a regression, not a fix.
#[test]
fn a_foreign_catalog_key_can_never_escape_the_destination_prefix() {
    let hex = "0".repeat(64);
    let digest_key = format!("foo/bar@sha256:{hex}");
    let over_long = format!("foo/{}", "x".repeat(300));

    let cases: &[(&str, &str)] = &[
        ("foo/../../prod-images", "`..` traversal out of the prefix"),
        ("foo/..", "a trailing `..` segment"),
        ("../prod-images", "a leading `..` segment"),
        ("foo/.", "a `.` segment"),
        ("./foo", "a leading `.` segment"),
        ("/prod-images", "an absolute path"),
        ("prod-images/", "a trailing separator"),
        ("foo//bar", "an empty interior segment"),
        ("Foo/Bar", "uppercase, which must be refused and never lowercased"),
        ("foo/bar:latest", "a smuggled tag"),
        (digest_key.as_str(), "a smuggled digest"),
        ("..\\..\\prod/images", "a Windows-shaped traversal"),
        ("foo/bar\\baz", "a Windows path separator"),
        ("C:\\windows/system32", "a Windows drive-qualified path"),
        ("foo/pkgé", "a non-ASCII segment"),
        ("foo/pk g", "whitespace"),
        ("foo/bar%2e%2e", "percent-encoded traversal"),
        ("foo/bar\nX-Injected: 1", "an embedded newline"),
        (
            "foo/\u{212a}elvin",
            "U+212A KELVIN SIGN, which `to_lowercase` folds onto `k`",
        ),
        ("foo/bar\u{202e}", "a right-to-left override"),
        (over_long.as_str(), "a path past the 255-character cap"),
        ("", "an empty key"),
        ("prod-images", "a single-segment key"),
    ];

    for (key, shape) in cases {
        let Err(message) = destination("{namespace}/{package}", "ocx.sh", "mirror", key) else {
            panic!("`{key}` ({shape}) must be refused, never repaired");
        };
        assert!(
            !message.is_empty(),
            "`{key}` ({shape}) must be refused with a message that names the fault"
        );
    }
}

/// The same table one level up: a hostile `as:` cannot escape either. `as:` is
/// operator-authored, so this is defence in depth rather than the trust
/// boundary proper — but the expansion is composed from both halves, and only
/// the composed value is validated.
#[test]
fn the_registry_expansion_is_validated_on_the_composed_value_too() {
    for (registry_as, shape) in [
        ("..", "a traversing `as:`"),
        ("OCX.SH", "an uppercase `as:`"),
        ("ocx.sh:443", "a port in `as:`"),
        ("", "an empty `as:`"),
    ] {
        assert!(
            destination(
                "{registry}/{namespace}/{package}",
                registry_as,
                "mirror",
                "kitware/cmake"
            )
            .is_err(),
            "`as: {registry_as}` ({shape}) must be refused once composed"
        );
    }
}

/// A template that substitutes nothing must not land the destination *on* the
/// prefix. The mirror writes strictly inside `target.repository`; a package
/// whose destination is the prefix itself would overwrite the operator's own
/// namespace root.
#[test]
fn an_empty_expansion_cannot_land_on_the_prefix_itself() {
    let expanded = DestinationTemplate::parse("")
        .expect("an empty template parses")
        .expand("ocx.sh", "kitware/cmake", None)
        .expect("a well-formed key");
    assert_eq!(expanded, "");

    let Err(error) = physical_repository(&target("mirror"), &expanded) else {
        panic!("an empty expansion must not resolve to the prefix itself");
    };
    assert!(
        error.to_string().contains("mirror/"),
        "the message must show the composed value that was refused: {error}"
    );
}

#[test]
fn a_legal_catalog_key_lands_strictly_inside_the_prefix() {
    let cases: &[(&str, &str, &str, &str, &str)] = &[
        (
            "{namespace}/{package}",
            "ocx.sh",
            "mirror",
            "kitware/cmake",
            "mirror/kitware/cmake",
        ),
        (
            "{registry}/{namespace}/{package}",
            "ocx.sh",
            "mirror",
            "kitware/cmake",
            "mirror/ocx.sh/kitware/cmake",
        ),
        (
            "{registry}/{namespace}/{package}",
            "ghcr.io",
            "ocx-contrib/mirror",
            "a-b_c.d/e1",
            "ocx-contrib/mirror/ghcr.io/a-b_c.d/e1",
        ),
        (
            "{registry}-{namespace}/{package}",
            "ocx.sh",
            "mirror",
            "kitware/cmake",
            "mirror/ocx.sh-kitware/cmake",
        ),
    ];

    for (template, registry_as, prefix, key, expected) in cases {
        let composed =
            destination(template, registry_as, prefix, key).unwrap_or_else(|error| panic!("`{key}` is legal: {error}"));

        assert_eq!(composed, *expected);
        assert!(
            composed.starts_with(&format!("{prefix}/")),
            "`{composed}` must sit strictly inside `{prefix}`"
        );
        assert_ne!(composed, *prefix, "`{composed}` must not be the prefix itself");
    }
}

// ── C-014 — the `oci://` pointer ────────────────────────────────────────────

#[test]
fn the_pointer_round_trips_through_the_parser_every_consumer_uses() {
    let cases: &[(&str, &str)] = &[
        ("ghcr.io", "mirror/kitware/cmake"),
        ("localhost:5002", "mirror/kitware/cmake"),
        ("registry.internal.example.com", "ocx-contrib/mirror/a-b_c.d/e1"),
    ];

    for (registry, physical) in cases {
        let target = Target {
            registry: (*registry).to_string(),
            repository: "mirror".to_string(),
        };

        let pointer =
            wire_pointer(&target, physical).unwrap_or_else(|error| panic!("`{registry}/{physical}`: {error}"));
        assert_eq!(pointer, format!("oci://{registry}/{physical}"));

        let (host, path) = parse_physical_repository(&pointer)
            .unwrap_or_else(|error| panic!("the pointer this function returned must re-parse: {error}"));
        assert_eq!(host, *registry);
        assert_eq!(path, *physical);
    }
}

/// C-014's whole reason for existing: reach the consumer's verdict here, not at
/// resolve time on someone else's machine. Each row is a pointer shape
/// `parse_physical_repository` refuses.
#[test]
fn a_pointer_the_consumer_would_reject_is_refused_before_it_is_written() {
    let hex = "0".repeat(64);
    let digest_physical = format!("mirror/cmake@sha256:{hex}");

    let cases: &[(&str, &str, &str)] = &[
        ("", "mirror/cmake", "an empty registry leaves no host"),
        ("ghcr.io", "", "an empty repository leaves no path"),
        ("ghcr.io", "mirror/cmake:latest", "a smuggled tag"),
        ("ghcr.io", digest_physical.as_str(), "a smuggled digest"),
        ("ghcr.io", "mirror/CMAKE", "an uppercase repository"),
        ("ghcr.io", "mirror/cm ake", "whitespace in the repository"),
        ("ghcr.io", "mirror/../cmake", "a traversal in the repository"),
    ];

    for (registry, physical, shape) in cases {
        let target = Target {
            registry: (*registry).to_string(),
            repository: "mirror".to_string(),
        };

        assert!(
            wire_pointer(&target, physical).is_err(),
            "`oci://{registry}/{physical}` ({shape}) must be refused before it reaches a root document"
        );
    }
}

// ── C-015 — collision detection ─────────────────────────────────────────────

fn expansion(source: &str, catalog_key: &str, repository: &str) -> Expansion {
    Expansion {
        source: source.to_string(),
        catalog_key: catalog_key.to_string(),
        repository: repository.to_string(),
    }
}

#[test]
fn distinct_destinations_are_not_a_collision() {
    let expansions = [
        expansion("ocx.sh", "kitware/cmake", "mirror/ocx.sh/kitware/cmake"),
        expansion("ghcr.io", "kitware/cmake", "mirror/ghcr.io/kitware/cmake"),
        expansion("ocx.sh", "bazelbuild/bazelisk", "mirror/ocx.sh/bazelbuild/bazelisk"),
    ];

    assert!(detect_collisions(&expansions).is_ok());
    assert!(detect_collisions(&[]).is_ok(), "an empty run collides with nothing");
}

/// The same package listed twice is a duplicate, not a collision — C-015
/// refuses two **distinct** keys sharing a destination.
#[test]
fn the_same_package_twice_is_not_a_collision() {
    let expansions = [
        expansion("ocx.sh", "kitware/cmake", "mirror/kitware/cmake"),
        expansion("ocx.sh", "kitware/cmake", "mirror/kitware/cmake"),
    ];

    assert!(detect_collisions(&expansions).is_ok());
}

/// Two sources publishing the **same** catalog key are two different upstream
/// packages, and one destination cannot hold both.
///
/// This is what makes the source name part of the distinctness comparison and
/// not merely part of the message: comparing keys alone reads these two rows as
/// one package listed twice and lets the second silently overwrite the first.
#[test]
fn one_key_published_by_two_sources_is_a_collision_not_a_duplicate() {
    let expansions = [
        expansion("ocx.sh", "kitware/cmake", "mirror/kitware/cmake"),
        expansion("ghcr.io", "kitware/cmake", "mirror/kitware/cmake"),
    ];

    let Err(MirrorError::SpecInvalid(messages)) = detect_collisions(&expansions) else {
        panic!("two sources cannot both own 'mirror/kitware/cmake'");
    };

    assert_eq!(messages.len(), 1);
    for needle in ["ocx.sh", "ghcr.io", "mirror/kitware/cmake"] {
        assert!(
            messages[0].contains(needle),
            "the message must name `{needle}` to tell the two sides apart: {}",
            messages[0]
        );
    }
}

#[test]
fn two_keys_from_one_source_sharing_a_destination_are_refused() {
    let expansions = [
        expansion("ocx.sh", "kitware/cmake", "mirror/cmake"),
        expansion("ocx.sh", "other/cmake", "mirror/cmake"),
    ];

    let Err(MirrorError::SpecInvalid(messages)) = detect_collisions(&expansions) else {
        panic!("two distinct keys sharing 'mirror/cmake' must be refused");
    };

    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    for needle in ["mirror/cmake", "kitware/cmake", "other/cmake"] {
        assert!(message.contains(needle), "the message must name `{needle}`: {message}");
    }
}

/// The cross-source case, built through the real pipeline rather than by hand.
///
/// `{registry}` is present, `as:` values are distinct, and every catalog key is
/// distinct — all three of C-006's structural guards hold — and the two still
/// collide, because a template may place `{registry}` next to a literal instead
/// of on a path boundary. This is why the collision check is whole-run and why
/// its input carries the source name: without it both sides of the message read
/// as the same package.
#[test]
fn two_sources_colliding_through_a_literal_boundary_are_refused_and_both_named() {
    let template = "{registry}-{namespace}/{package}";
    let prefix = "mirror";

    let first = destination(template, "ocx.sh", prefix, "b-c/d").expect("a legal destination");
    let second = destination(template, "ocx.sh-b", prefix, "c/d").expect("a legal destination");
    assert_eq!(first, second, "precondition: the two sources really do collide");

    let expansions = [
        expansion("ocx.sh", "b-c/d", &first),
        expansion("ocx.sh-b", "c/d", &second),
    ];

    let Err(MirrorError::SpecInvalid(messages)) = detect_collisions(&expansions) else {
        panic!("`{first}` is claimed by two different sources and must be refused");
    };

    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    for needle in ["ocx.sh", "ocx.sh-b", "b-c/d", "c/d", first.as_str()] {
        assert!(message.contains(needle), "the message must name `{needle}`: {message}");
    }
}

/// One run, one report: an operator fixing a multi-source spec should not have
/// to re-run to discover the second clash.
#[test]
fn every_collision_in_a_run_is_reported_not_only_the_first() {
    let expansions = [
        expansion("a", "one/x", "mirror/x"),
        expansion("b", "two/x", "mirror/x"),
        expansion("a", "one/y", "mirror/y"),
        expansion("b", "two/y", "mirror/y"),
    ];

    let Err(MirrorError::SpecInvalid(messages)) = detect_collisions(&expansions) else {
        panic!("two separate collisions must be refused");
    };

    assert_eq!(messages.len(), 2, "both collisions must be reported: {messages:?}");
    assert!(messages[0].contains("mirror/x"));
    assert!(messages[1].contains("mirror/y"));
}

/// Case is never folded when comparing destinations — it does not need to be,
/// because [`physical_repository`] already refused uppercase outright. Folding
/// here would be the second half of the KELVIN-SIGN collision the grammar guard
/// exists to prevent.
#[test]
fn collision_comparison_folds_no_case() {
    let expansions = [expansion("a", "one/x", "mirror/x"), expansion("b", "two/x", "mirror/X")];

    assert!(
        detect_collisions(&expansions).is_ok(),
        "these are different strings; `mirror/X` never survives the grammar guard to reach here"
    );
}

// ── CWE-117 — foreign strings never reach a message raw ─────────────────────

/// A catalog key is authored by the source and reaches these messages **before**
/// any charset guard has judged it.
///
/// Echoed raw, a key of `cmake\n[2026-08-14 INFO] copied 121/121 packages ok`
/// forges a log line in the CI output an operator reads, and a `\u{202e}`
/// reverses what their terminal shows. Every site that names one formats it
/// `{:?}`, which escapes both classes.
#[test]
fn a_refused_key_is_escaped_into_every_message_that_names_it() {
    // No `/` in the forged line, so the key is refused for the shape it has
    // rather than accidentally splitting into two plausible segments.
    let forged = "cmake\n[2026-08-14 INFO] copied 121 of 121 packages ok";
    let template = DestinationTemplate::parse("{namespace}/{package}").expect("the template parses");

    // C-012 — the malformed-key refusal, the earliest site of all.
    let malformed = template
        .expand("ocx.sh", forged, None)
        .expect_err("a single-segment key is refused")
        .to_string();
    assert!(
        !malformed.contains('\n') && malformed.contains("\\n"),
        "the newline must be escaped, not echoed: {malformed:?}"
    );

    let spoofed = template
        .expand("ocx.sh", "cmake\u{202e}", None)
        .expect_err("still a single-segment key")
        .to_string();
    assert!(
        !spoofed.contains('\u{202e}') && spoofed.contains("\\u{202e}"),
        "the direction override must be escaped: {spoofed:?}"
    );

    // C-013 — the grammar guard, which names the composed value. Asserted on
    // the message rather than the rendered error: `SpecInvalid`'s own `Display`
    // is line-structured, and that structure is exactly what a forged newline
    // would break out of.
    let grammar =
        messages(physical_repository(&target("mirror"), forged).expect_err("a newline is not in the grammar"));
    assert!(
        !grammar[0].contains('\n') && grammar[0].contains("\\n"),
        "the composed value must be escaped: {:?}",
        grammar[0]
    );

    // C-015 — the collision report, which names two keys and a repository.
    let collision = messages(
        detect_collisions(&[expansion("a", forged, "mirror/x"), expansion("b", "two/x", "mirror/x")])
            .expect_err("a collision is refused"),
    );
    assert!(
        !collision[0].contains('\n') && collision[0].contains("\\n"),
        "a colliding key must be escaped too: {:?}",
        collision[0]
    );
}

/// The individual messages inside a [`MirrorError::SpecInvalid`], which is the
/// only variant this module produces.
fn messages(error: MirrorError) -> Vec<String> {
    match error {
        MirrorError::SpecInvalid(messages) => messages,
        other => panic!("every refusal here is SpecInvalid: {other:?}"),
    }
}

// ── Preserved-pointer reachability ──────────────────────────────────────────

/// The reachable shapes, which are exactly "the landing path ends in the
/// upstream repository, on a segment boundary".
///
/// Both prefix depths are here because they are the two the shipped templates
/// actually produce: a single-source spec drops `{registry}` and lands one
/// segment under the prefix, the documented multi-source one keeps it and lands
/// two. An equality check against `target.repository` would pass the first and
/// warn on the second, which is why the rule is a tail match.
#[test]
fn a_landing_path_ending_in_the_upstream_repository_is_reachable() {
    for (physical, repository) in [
        // `path_prefix = "ocx-mirror"`.
        ("ocx-mirror/kubernetes/kubectl", "kubernetes/kubectl"),
        // `path_prefix = "ocx-mirror/ocx.sh"` — the documented example.
        ("ocx-mirror/ocx.sh/kubernetes/kubectl", "kubernetes/kubectl"),
        // A host-only mirror: no prefix at all, the path is the repository.
        ("kubernetes/kubectl", "kubernetes/kubectl"),
        // Depth is not the rule; the tail is. An upstream repository of any
        // number of segments works the same way.
        ("corp/a/b/c/deep", "a/b/c/deep"),
    ] {
        assert!(
            !mirror_path_mismatch(physical, repository),
            "{physical:?} is reachable from {repository:?} through a path prefix"
        );
    }
}

/// The unreachable shapes — no `path_prefix` value resolves any of these, so
/// the warning is a statement of fact rather than a guess.
///
/// The indirected case is the one that motivates the check at all: the root's
/// `repository` is transport-only and may name a path unrelated to the catalog
/// key the template expanded, so the copy lands somewhere no client asks for.
#[test]
fn a_landing_path_that_does_not_end_in_the_upstream_repository_is_refused() {
    for (physical, repository) in [
        // Indirection: the catalog key expanded to `kitware/cmake`, but the
        // source serves the content from an unrelated physical path.
        ("mirror/kitware/cmake", "ocx-contrib/cmake-releases"),
        // A suffix match that is not a segment match — `not` is not a prefix
        // any registry serves the way this reads.
        ("corp/notkubectl", "kubectl"),
        // The tail is right but the order is not.
        ("kubernetes/kubectl/corp", "kubernetes/kubectl"),
        // A leading-slash path is not "the empty prefix"; that case is the
        // equality above.
        ("/kubernetes/kubectl", "kubernetes/kubectl"),
    ] {
        assert!(
            mirror_path_mismatch(physical, repository),
            "{physical:?} is not reachable from {repository:?} through any path prefix"
        );
    }
}

// ── Exit-code class ─────────────────────────────────────────────────────────

/// Every refusal in this module is plan-time, malformed-input, exit 65.
#[test]
fn every_destination_refusal_is_a_data_error() {
    use ocx_lib::cli::ExitCode;

    let grammar = physical_repository(&target("mirror"), "foo/..").expect_err("a traversal is refused");
    let pointer = wire_pointer(&target("mirror"), "mirror/cmake:latest").expect_err("a tagged pointer is refused");
    let collision = detect_collisions(&[expansion("a", "one/x", "mirror/x"), expansion("b", "two/x", "mirror/x")])
        .expect_err("a collision is refused");

    for error in [grammar, pointer, collision] {
        assert_eq!(error.kind_exit_code(), ExitCode::DataError, "{error}");
    }
}
