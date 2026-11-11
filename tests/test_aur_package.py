"""`scripts/aur_package.py`: the AUR package is written whole from the release it pins."""

import importlib.util
import subprocess
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


# Responses of the AUR's RPC (`/rpc/v5/info?arg[]=fastbrowse`) in the shape it serves them: the first is what it
# answers while the AUR has no fastbrowse, the second carries the fields of one package's result.
AUR_WITHOUT_FASTBROWSE = {"resultcount": 0, "results": [], "type": "multiinfo", "version": 5}
AUR_WITH_FASTBROWSE = {
    "resultcount": 1,
    "results": [
        {
            "Name": "fastbrowse",
            "PackageBase": "fastbrowse",
            "Version": "9.9.9-2",
            "URLPath": "/cgit/aur.git/snapshot/fastbrowse.tar.gz",
            "OutOfDate": None,
        }
    ],
    "type": "multiinfo",
    "version": 5,
}


def test_the_aur_version_is_its_pkgver_without_the_pkgrel() -> None:
    assert aur_package.aur_version(AUR_WITH_FASTBROWSE) == "9.9.9"
    assert aur_package.aur_version(AUR_WITHOUT_FASTBROWSE) is None


def test_the_check_fails_when_the_aur_lacks_the_pypi_release() -> None:
    # The committed package can be current while nothing was ever pushed to the AUR, which is what a release
    # without the AUR key leaves behind, so the check reads the AUR itself.
    problems = aur_package.check(NEW, NEW, aur_package.aur_version(AUR_WITHOUT_FASTBROWSE))

    assert problems == ["the AUR has no fastbrowse, and PyPI has 9.9.9"]


def test_the_check_fails_when_the_aur_is_a_release_behind() -> None:
    assert aur_package.check(NEW, NEW, "9.9.8") == ["the AUR has fastbrowse 9.9.8, and PyPI has 9.9.9"]


def test_the_check_names_a_committed_package_and_an_aur_that_are_both_behind() -> None:
    old = aur_package.Release("9.9.8", "c" * 64)

    assert aur_package.check(old, NEW, "9.9.8") == [
        "packaging/aur is fastbrowse 9.9.8, and PyPI has 9.9.9",
        "the AUR has fastbrowse 9.9.8, and PyPI has 9.9.9",
    ]


def test_the_check_passes_when_the_aur_serves_the_pypi_release() -> None:
    assert aur_package.check(NEW, NEW, aur_package.aur_version(AUR_WITH_FASTBROWSE)) == []


def test_the_first_publish_creates_the_package_in_an_empty_aur_repository(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The AUR serves a package that does not exist yet as an empty repository, and the first push creates it.
    remote = tmp_path / "aur.git"
    subprocess.run(["git", "init", "--quiet", "--bare", str(remote)], check=True)
    config = tmp_path / "gitconfig"
    config.write_text("[user]\n\tname = fixture\n\temail = fixture@example.invalid\n[commit]\n\tgpgsign = false\n")
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(config))
    monkeypatch.setenv("GIT_CONFIG_NOSYSTEM", "1")
    monkeypatch.setattr(aur_package, "AUR_REMOTE", f"file://{remote}")
    package = tmp_path / "package"
    package.mkdir()
    for name, text in aur_package.render(NEW).items():
        (package / name).write_text(text, encoding="utf-8")

    assert aur_package.publish(package, NEW) == 0

    shown = subprocess.run(
        ["git", "--git-dir", str(remote), "show", "master:.SRCINFO"], capture_output=True, text=True, check=True
    )
    assert shown.stdout == aur_package.render(NEW)[".SRCINFO"]
