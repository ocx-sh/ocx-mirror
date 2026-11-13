# Skips on the macos-release leg

One line per skipped test id (`<classname>.<name>`) and its reason; `scripts/accept_floor.py` fails on a skipped id missing here.

- tests.test_mirror_patch_legacy.test_patch_keeps_the_pre_patch_image_index_resolvable: no docker: OCX_TEST_NO_DOCKER=1, legacy_registry (the compose 'legacy_registry') is not started
- tests.test_mirror_pylock.test_pylock_pipeline_pushes_env_to_registry: e2e needs a real cpython on the registry — validated in W3 against dev.ocx.sh
- tests.test_mirror_pypi.test_plan_discovers_versions_from_real_pypi: requires OCX_TESTS_ONLINE=1 and network access
- tests.test_ocx_spawn_sites.test_pipeline_announce_curates_the_registry_tags_into_the_index: the toolchain ocx is not an ocx `__testing` build, so it ignores the fake-forge seam
- tests.test_registry_sync.test_a_package_carrying_a_referrer_is_copied_and_listed_in_the_fallback_index: no docker: OCX_TEST_NO_DOCKER=1, legacy_registry (the compose 'legacy_registry') is not started
- tests.test_registry_sync.test_one_referrer_reached_through_two_tags_is_copied_once: no docker: OCX_TEST_NO_DOCKER=1, legacy_registry (the compose 'legacy_registry') is not started
- tests.test_registry_sync_harness.test_legacy_registry_does_not_answer_the_referrers_api: no docker: OCX_TEST_NO_DOCKER=1, legacy_registry (the compose 'legacy_registry') is not started
- tests.test_registry_sync_harness.test_mint_identity_token_writes_a_private_jwt: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_registry_sync_harness.test_registry_port_belongs_to_the_session_zot: resolves the listener through /proc
- tests.test_registry_sync_harness.test_wait_for_sigstore_leaves_every_polled_endpoint_answering: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_a_mirror_without_a_sign_block_publishes_unsigned_and_green: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_a_second_push_adds_one_signature_not_one_per_tag: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_a_second_run_that_moves_the_index_signs_the_new_one: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_a_sign_block_with_no_reachable_identity_fails_before_publishing: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_key_mode_signs_with_the_named_key: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_keyless_signs_every_platform_manifest_and_the_index: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_package_sync_signs_the_leg_it_publishes_in_process: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing.test_patch_signs_the_index_it_republishes: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_a_second_run_signs_nothing: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_dry_run_reports_a_verdict_and_issues_no_child: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_force_signs_an_already_signed_subject: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_identity_narrowing_reads_the_signer_off_the_live_signature: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_one_run_signs_each_distinct_subject_once: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_repeated_runs_stay_within_the_candidate_budget: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_sigint_mid_batch_accounts_for_every_subject: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_backfill.test_the_backfill_signs_the_index_and_the_platform: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_dispatch.test_identity_token_under_the_ocx_owned_name_is_refused_before_anything_publishes: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_dispatch.test_key_env_under_an_ocx_owned_name_is_refused_before_anything_publishes: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_dispatch.test_key_env_under_an_operator_named_variable_survives_the_dispatch: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_signing_dispatch.test_the_plugin_dispatch_shape_still_signs: no docker: OCX_TEST_NO_DOCKER=1, the Sigstore stack is not started
- tests.test_version.test_json_carries_the_placeholder_provenance[format-json]: release bytes: the release binary lacks the `__testing` seams this test drives
- tests.test_version.test_json_carries_the_placeholder_provenance[json]: release bytes: the release binary lacks the `__testing` seams this test drives
- tests.test_version.test_verbose_plain_names_the_placeholder_commit_and_channel: release bytes: the release binary lacks the `__testing` seams this test drives
