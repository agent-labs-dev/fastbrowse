"""`scripts/aur_package.py`: moving the AUR package to a release changes the pins and nothing else."""

import importlib.util
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

_spec = importlib.util.spec_from_file_location("aur_package", REPO / "scripts" / "aur_package.py")
assert _spec and _spec.loader
aur_package = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(aur_package)

NEW = aur_package.Release("9.9.9", "8.8.8", ("a" * 64, "b" * 64))


def _committed(name: str) -> str:
    return (REPO / "packaging" / "aur" / name).read_text(encoding="utf-8")


def test_both_files_pin_the_same_release() -> None:
    old = aur_package.pinned(_committed("PKGBUILD"))
    srcinfo = _committed(".SRCINFO")

    assert f"\tpkgver = {old.version}\n" in srcinfo
    assert f"/cdp_use-{old.cdp_use}.tar.gz" in srcinfo
    assert [line.split(" = ")[1] for line in srcinfo.splitlines() if "sha256sums" in line] == list(old.sums)


def test_a_new_release_moves_every_pin_in_both_files() -> None:
    old = aur_package.pinned(_committed("PKGBUILD"))

    pkgbuild = aur_package.rewrite(_committed("PKGBUILD"), old, NEW)
    srcinfo = aur_package.rewrite(_committed(".SRCINFO"), old, NEW)

    assert aur_package.pinned(pkgbuild) == NEW
    for text in (pkgbuild, srcinfo):
        assert old.sums[0] not in text and old.sums[1] not in text
    assert "\tpkgver = 9.9.9\n" in srcinfo
    assert "/fastbrowse-9.9.9.tar.gz" in srcinfo
    assert "/cdp_use-8.8.8.tar.gz" in srcinfo
    assert old.version not in srcinfo and old.cdp_use not in srcinfo


def test_a_new_release_starts_from_its_first_build() -> None:
    old = aur_package.pinned(_committed("PKGBUILD"))
    rebuilt = _committed("PKGBUILD").replace("pkgrel=1", "pkgrel=3")

    assert "\npkgrel=1\n" in aur_package.rewrite(rebuilt, old, NEW)


def test_equal_version_numbers_stay_with_their_own_wheel() -> None:
    old = aur_package.pinned(_committed("PKGBUILD"))
    same = aur_package.Release("7.7.7", "7.7.7", NEW.sums)

    moved = aur_package.rewrite(aur_package.rewrite(_committed(".SRCINFO"), old, same), same, NEW)

    assert "/fastbrowse-9.9.9.tar.gz" in moved
    assert "/cdp_use-8.8.8.tar.gz" in moved
