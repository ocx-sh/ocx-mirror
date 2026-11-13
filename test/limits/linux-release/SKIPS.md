# Skips on the linux-release leg

One line per skipped test id (`<classname>.<name>`) and its reason; `scripts/accept_floor.py` fails on a skipped id missing here.

- tests.test_mirror_pylock.test_pylock_pipeline_pushes_env_to_registry: e2e needs a real cpython on the registry, validated against dev.ocx.sh
- tests.test_mirror_pypi.test_plan_discovers_versions_from_real_pypi: requires OCX_TESTS_ONLINE=1 and network access
- tests.test_ocx_spawn_sites.test_pipeline_announce_curates_the_registry_tags_into_the_index: the toolchain ocx is not an ocx `__testing` build, so it ignores the fake-forge seam
- tests.test_version.test_json_carries_the_placeholder_provenance[format-json]: release bytes lack the `__testing` provenance
- tests.test_version.test_json_carries_the_placeholder_provenance[json]: release bytes lack the `__testing` provenance
- tests.test_version.test_verbose_plain_names_the_placeholder_commit_and_channel: release bytes lack the `__testing` provenance
