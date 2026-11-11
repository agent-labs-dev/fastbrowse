"""`scripts/aur_package.py`: the AUR package is written whole from the release it pins."""

import importlib.util
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parent.parent

_spec = importlib.util.spec_from_file_location("aur_package", REPO / "scripts" / "aur_package.py")
assert _spec and _spec.loader
aur_package = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(aur_package)

NEW = aur_package.Release("9.9.9", "a" * 64)


def _committed(name: str) -> str:
    return (REPO / "packaging" / "aur" / name).read_text(encoding="utf-8")


def test_the_committed_package_pins_one_release_in_both_files() -> None:
    old = aur_package.pinned(_committed("PKGBUILD"))
    srcinfo = _committed(".SRCINFO")

    assert f"\tpkgver = {old.version}\n" in srcinfo
    assert f"\tsha256sums = {old.sha256}\n" in srcinfo


def test_a_release_is_written_into_both_files() -> None:
    written = aur_package.render(NEW)

    assert aur_package.pinned(written["PKGBUILD"]) == NEW
    assert "\tpkgver = 9.9.9\n\tpkgrel = 1\n" in written[".SRCINFO"]
    assert "/fastbrowse-9.9.9.tar.gz\n" in written[".SRCINFO"]
    assert f"\tsha256sums = {NEW.sha256}\n" in written[".SRCINFO"]


def test_the_package_builds_fastbrowse_alone() -> None:
    written = aur_package.render(NEW)

    for text in written.values():
        assert "cdp" not in text
        assert "mcp-" not in text
    assert written["PKGBUILD"].count("source=") == 1


def test_a_release_that_still_needs_cdp_use_is_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    record = {
        "info": {"name": "fastbrowse", "version": "0.8.1", "requires_dist": ["cdp-use==1.4.5", "httpx>=0.28"]},
        "urls": [{"packagetype": "sdist", "digests": {"sha256": "b" * 64}}],
    }
    monkeypatch.setattr(aur_package, "_pypi", lambda *_: record)

    with pytest.raises(SystemExit, match="still needs cdp-use"):
        aur_package.published("0.8.1")


def test_a_release_without_the_python_core_is_read_off_pypi(monkeypatch: pytest.MonkeyPatch) -> None:
    record = {
        "info": {"name": "fastbrowse", "version": "9.9.9", "requires_dist": ["httpx[http2]>=0.28", "pydantic>=2.11"]},
        "urls": [{"packagetype": "sdist", "digests": {"sha256": NEW.sha256}}],
    }
    monkeypatch.setattr(aur_package, "_pypi", lambda *_: record)

    assert aur_package.published("9.9.9") == NEW


def test_a_source_is_named_by_the_stable_url_the_package_fetches() -> None:
    # The PKGBUILD names its source by this address, so this is the one a release has to wait for.
    assert (
        aur_package.source_url("fastbrowse", "fastbrowse-0.7.1.tar.gz")
        == "https://files.pythonhosted.org/packages/source/f/fastbrowse/fastbrowse-0.7.1.tar.gz"
    )
    assert "/fastbrowse-9.9.9.tar.gz" in aur_package.render(NEW)["PKGBUILD"].replace("$pkgver", "9.9.9")


def test_the_package_files_the_repository_commits_are_the_ones_the_aur_is_compared_on() -> None:
    committed = {name: _committed(name) for name in aur_package.FILES}

    assert aur_package.changed(committed, committed) == []
    assert aur_package.changed({}, committed) == list(aur_package.FILES)


def test_a_publish_rewrites_only_the_files_that_differ() -> None:
    committed = {name: _committed(name) for name in aur_package.FILES}

    assert aur_package.changed(committed, {**committed, "PKGBUILD": aur_package.render(NEW)["PKGBUILD"]}) == [
        "PKGBUILD"
    ]


# Bodies of the shape the AUR RPC (`/rpc/v5/info`) returns, from the Arch wiki page "Aurweb RPC interface".
AUR_NONE = {"resultcount": 0, "results": [], "type": "multiinfo", "version": 5}
AUR_ERROR = {
    "error": "Incorrect request type specified.",
    "resultcount": 0,
    "results": [],
    "type": "error",
    "version": 5,
}


def _aur_holding(version: str) -> dict:
    return {
        "resultcount": 1,
        "results": [{"Name": "fastbrowse", "Version": version}],
        "type": "multiinfo",
        "version": 5,
    }


def test_the_aur_version_is_read_from_the_rpc() -> None:
    assert aur_package.aur_version(AUR_NONE) is None
    assert aur_package.aur_version(_aur_holding("0.10.1-1")) == "0.10.1"
    assert aur_package.aur_version(_aur_holding("1:0.10.1-2")) == "0.10.1"


def test_an_rpc_error_stops_the_check_with_its_own_message() -> None:
    with pytest.raises(SystemExit, match="Incorrect request type specified"):
        aur_package.aur_version(AUR_ERROR)


def _checkout(root: Path, pinned: aur_package.Release, aur: dict, monkeypatch: pytest.MonkeyPatch) -> None:
    (root / "pyproject.toml").write_text('[project]\nname = "fastbrowse"\nversion = "9.9.9"\n', encoding="utf-8")
    directory = root / "packaging" / "aur"
    directory.mkdir(parents=True)
    for name, text in aur_package.render(pinned).items():
        (directory / name).write_text(text, encoding="utf-8")
    record = {
        "info": {"name": "fastbrowse", "version": "9.9.9", "requires_dist": ["httpx[http2]>=0.28", "pydantic>=2.11"]},
        "urls": [{"packagetype": "sdist", "digests": {"sha256": NEW.sha256}}],
    }
    monkeypatch.setattr(aur_package, "_pypi", lambda *_: record)
    monkeypatch.setattr(aur_package, "_aur_info", lambda _: aur)
    monkeypatch.setattr("sys.argv", ["aur_package.py", "--root", str(root)])


def test_the_check_fails_while_the_aur_lacks_the_release(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    _checkout(tmp_path, NEW, AUR_NONE, monkeypatch)

    assert aur_package.main() == 1
    assert "the AUR has no fastbrowse, and PyPI has 9.9.9" in capsys.readouterr().err


def test_the_check_passes_when_the_aur_holds_the_release(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _checkout(tmp_path, NEW, _aur_holding("9.9.9-1"), monkeypatch)

    assert aur_package.main() == 0


def test_one_check_reports_a_stale_package_and_a_missing_aur_release(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    _checkout(tmp_path, aur_package.Release("9.9.8", "c" * 64), AUR_NONE, monkeypatch)

    assert aur_package.main() == 1
    err = capsys.readouterr().err
    assert "packaging/aur is fastbrowse 9.9.8, and PyPI has 9.9.9" in err
    assert "the AUR has no fastbrowse, and PyPI has 9.9.9" in err
