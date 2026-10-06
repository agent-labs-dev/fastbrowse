import sys

import pytest

from fastbrowse.spawn import system_environment


def test_a_checkout_hands_a_program_it_starts_its_own_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("LD_LIBRARY_PATH", "/opt/mine")

    assert system_environment() is None


def test_a_frozen_build_gives_back_the_library_path_it_was_started_with(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sys, "frozen", True, raising=False)
    monkeypatch.setenv("LD_LIBRARY_PATH", "/bundle/_internal")
    monkeypatch.setenv("LD_LIBRARY_PATH_ORIG", "/opt/mine")
    monkeypatch.setenv("HOME", "/home/someone")

    environment = system_environment()

    assert environment is not None
    assert environment["LD_LIBRARY_PATH"] == "/opt/mine"
    assert environment["HOME"] == "/home/someone"


def test_a_frozen_build_started_with_no_library_path_passes_none_on(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sys, "frozen", True, raising=False)
    monkeypatch.setenv("LD_LIBRARY_PATH", "/bundle/_internal")
    monkeypatch.delenv("LD_LIBRARY_PATH_ORIG", raising=False)

    environment = system_environment()

    assert environment is not None
    assert "LD_LIBRARY_PATH" not in environment
