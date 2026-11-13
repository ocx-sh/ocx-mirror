"""Pins two harness start-up contracts: no Docker fails loudly, and only the controller starts zot."""

from __future__ import annotations

from types import SimpleNamespace

import pytest
from _pytest.outcomes import Failed

import conftest
from src import helpers


def _fixture_body(fixture):
    # pytest 8 wraps a fixture in a definition object; 9 keeps the function on it.
    return getattr(fixture, "_get_wrapped_function", lambda: fixture.__wrapped__)()


def test_legacy_registry_fails_not_skips_without_docker(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(helpers.NO_DOCKER_ENV, raising=False)
    monkeypatch.delenv("OCX_TESTS_NO_REGISTRY", raising=False)
    monkeypatch.setattr(conftest.shutil, "which", lambda _name: None)
    monkeypatch.setattr(conftest, "registry_is_reachable", lambda _address: False)
    # Skip is caught too: uncaught, a fail->skip regression would report this test skipped, not failed.
    with pytest.raises((Failed, pytest.skip.Exception)) as outcome:
        _fixture_body(conftest.legacy_registry)()
    assert outcome.type is Failed, f"the fixture must fail, not skip: {outcome.value}"
    assert "no `docker`" in str(outcome.value)


def test_legacy_registry_skips_when_the_job_declares_no_docker(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(helpers.NO_DOCKER_ENV, "1")
    monkeypatch.delenv("OCX_TESTS_NO_REGISTRY", raising=False)
    with pytest.raises(pytest.skip.Exception, match="no docker"):
        _fixture_body(conftest.legacy_registry)()


def _config(*, worker: bool) -> SimpleNamespace:
    config = SimpleNamespace(getoption=lambda _name: False)
    if worker:
        config.workerinput = {}
    return config


def test_zot_starts_on_the_controller_only(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("OCX_TESTS_NO_REGISTRY", raising=False)
    assert conftest._wants_zot(_config(worker=False)) is True
    assert conftest._wants_zot(_config(worker=True)) is False
