// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-012, C-013 and the collision half of C-015 — filter, expand, and the
//! `Expansion` set the whole-run collision check runs over.

use super::super::*;
use super::support::*;

#[test]
fn every_selected_key_expands_to_a_contained_repository() {
    let spec = spec("{registry}/{namespace}/{package}");
    let catalog = catalog(&["kitware/cmake", "ninja-build/ninja"]);

    let work = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog).expect("expand");

    assert_eq!(
        destinations(&work),
        vec![
            ("kitware/cmake".to_string(), "mirror/ocx.sh/kitware/cmake".to_string()),
            (
                "ninja-build/ninja".to_string(),
                "mirror/ocx.sh/ninja-build/ninja".to_string()
            ),
        ]
    );
    assert_eq!(work[0].pointer, "oci://registry.test/mirror/ocx.sh/kitware/cmake");
}

#[test]
fn the_work_list_follows_catalog_order() {
    // Deterministic without sorting anything: `CatalogIndex` is a `BTreeMap`,
    // so a run's work list is reproducible and a report diffs cleanly.
    let spec = spec("{registry}/{namespace}/{package}");
    let catalog = catalog(&["zebra/z", "alpha/a", "middle/m"]);

    let work = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog).expect("expand");

    assert_eq!(names(&work), vec!["alpha/a", "middle/m", "zebra/z"]);
}

#[test]
fn an_empty_include_selects_everything_and_exclude_vetoes() {
    let spec = spec("{registry}/{namespace}/{package}");
    let catalog = catalog(&["kitware/cmake", "kitware/ccmake", "ninja-build/ninja"]);

    let everything = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog).expect("expand");
    assert_eq!(everything.len(), 3);

    let narrowed = expand_source(&spec, &source(&["kitware/*"], &[]), "ocx.sh", &catalog).expect("expand");
    assert_eq!(names(&narrowed), vec!["kitware/ccmake", "kitware/cmake"]);

    // An exclude beats an include unconditionally.
    let vetoed = expand_source(&spec, &source(&["kitware/*"], &["*/ccmake"]), "ocx.sh", &catalog).expect("expand");
    assert_eq!(names(&vetoed), vec!["kitware/cmake"]);
}

#[test]
fn the_source_as_name_is_what_expands_registry() {
    // `as:` is the output subtree AND the `{registry}` expansion, so renaming
    // it re-homes every destination repository. Pinned so the coupling is
    // visible rather than discovered on a rename.
    let spec = spec("{registry}/{namespace}/{package}");
    let catalog = catalog(&["kitware/cmake"]);

    let first = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog).expect("expand");
    let renamed = expand_source(&spec, &source(&[], &[]), "internal", &catalog).expect("expand");

    assert_eq!(first[0].physical_repository, "mirror/ocx.sh/kitware/cmake");
    assert_eq!(renamed[0].physical_repository, "mirror/internal/kitware/cmake");
}

#[test]
fn a_catalog_key_that_escapes_the_prefix_is_refused_at_plan_time() {
    // The threat-model case: a compromised source registry serving a catalog
    // key crafted to climb out of the configured prefix.
    let spec = spec("{registry}/{namespace}/{package}");
    let catalog = catalog(&["foo/../../prod-images"]);

    let error = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog)
        .expect_err("a traversal key must never reach a destination");

    assert!(refusal_messages(&error).contains("prod-images"), "{error:?}");
}

#[test]
fn a_catalog_key_that_is_not_two_segments_is_refused() {
    let spec = spec("{registry}/{namespace}/{package}");

    for key in ["flat", "a/b/c", "/leading", "trailing/"] {
        let error = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog(&[key]))
            .expect_err("only <namespace>/<package> is a catalog key");
        assert!(refusal_messages(&error).contains(key), "{key}: {error:?}");
    }
}

#[test]
fn a_glob_outside_the_grammar_is_refused() {
    // `RegistrySpec::validate` should already have refused it at load time;
    // this is the belt-and-braces half, and it must not panic or match
    // everything.
    let spec = spec("{registry}/{namespace}/{package}");

    let error = expand_source(
        &spec,
        &source(&["kitware/**"], &[]),
        "ocx.sh",
        &catalog(&["kitware/cmake"]),
    )
    .expect_err("`**` is outside the glob grammar");

    assert!(refusal_messages(&error).contains("kitware/**"), "{error:?}");
}

#[test]
fn expansions_carry_the_source_so_two_sources_collide_rather_than_merge() {
    // C-015's distinctness is `(source, catalog_key)`. Two sources publishing
    // the same catalog key are two DIFFERENT upstream packages; building an
    // `Expansion` without `source` reads them as one package listed twice, so
    // no collision is reported and the second silently overwrites the first.
    let spec = spec("{namespace}/{package}");
    let catalog = catalog(&["kitware/cmake"]);
    let first = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog).expect("expand");
    let second = expand_source(&spec, &source(&[], &[]), "internal", &catalog).expect("expand");

    let mut all = expansions("ocx.sh", &first);
    all.extend(expansions("internal", &second));

    // Both sources expanded onto `mirror/kitware/cmake` — the template omits
    // `{registry}`, which is exactly the spec mistake C-015 exists to catch.
    assert_eq!(all[0].repository, all[1].repository);
    let error = super::super::super::destination::detect_collisions(&all).expect_err("two sources, one repository");
    let message = refusal_messages(&error);
    assert!(message.contains("ocx.sh") && message.contains("internal"), "{message}");
}

#[test]
fn one_source_seen_twice_is_a_duplicate_not_a_collision() {
    // The green counterpart: the same `(source, catalog_key)` pair appearing
    // twice is the same package, and refusing it would refuse every legal run.
    let spec = spec("{registry}/{namespace}/{package}");
    let work = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog(&["kitware/cmake"])).expect("expand");

    let mut all = expansions("ocx.sh", &work);
    all.extend(expansions("ocx.sh", &work));

    super::super::super::destination::detect_collisions(&all).expect("the same package twice is not a collision");
}

#[test]
fn plan_source_reports_the_whole_expansion_even_when_it_short_circuits() {
    // A short-circuited source is one already mirrored to these repositories.
    // Dropping it from the collision check would let another source claim the
    // same repository unnoticed, so `work` stays the full set and
    // `short_circuited` is a separate flag.
    let spec = spec("{registry}/{namespace}/{package}");
    let catalog = catalog(&["kitware/cmake"]);
    let local: CatalogIndex = [("kitware/cmake".to_string(), digest("root").to_string())]
        .into_iter()
        .collect();

    let plan = plan_source(
        &spec,
        &source(&[], &[]),
        "ocx.sh",
        &catalog,
        "sha256:cafe",
        Some(&super::super::cache_record("sha256:cafe", &local)),
        &local,
    )
    .expect("plan");

    assert!(plan.short_circuited, "an unchanged catalog with everything local");
    assert_eq!(
        names(&plan.work),
        vec!["kitware/cmake"],
        "the expansion is still reported"
    );
    assert_eq!(plan.as_name, "ocx.sh");
    assert!(
        plan.cache_record.starts_with("sha256:cafe "),
        "the record carries the source digest and the local fingerprint: {}",
        plan.cache_record
    );
}
