#!/usr/bin/env bash
# Build the AUR package in a clean Arch container, install it, and run what it installed.
#
#   docker run --rm -v "$PWD:/repo:ro" archlinux:base-devel bash /repo/packaging/aur/build.sh
#
# Runs as root inside the container and changes the system it runs on, so never run it on a real machine.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

pacman -Syu --noconfirm --needed base-devel

# makepkg refuses to run as root, and installs the dependencies it finds missing through sudo.
useradd --create-home builder
echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
work=$(sudo -u builder mktemp -d)
install -o builder -m644 "$here/PKGBUILD" "$here/.SRCINFO" "$work"
cd "$work"

# The AUR reads .SRCINFO and never the PKGBUILD, so a stale one publishes the previous version's metadata.
sudo -u builder makepkg --printsrcinfo | diff -u .SRCINFO -
# Everything makepkg installed to build is removed again, so the commands below run on what `depends` names
# and a missing entry there fails here.
sudo -u builder makepkg --syncdeps --rmdeps --noconfirm
pacman -U --noconfirm fastbrowse-*.pkg.tar.zst

cd /
fastbrowse --help > /dev/null
fastbrowse-mcp --help > /dev/null

# pacman knows only the lower bounds in `depends`. The installed metadata states the full ranges, so this
# fails when a package in the official repositories has moved outside one, or a release needs one `depends` lacks.
pacman -S --noconfirm --needed python-packaging
/opt/fastbrowse/bin/python - <<'EOF'
from importlib.metadata import PackageNotFoundError, distribution, version

from packaging.requirements import Requirement
from packaging.utils import canonicalize_name

failures = []
seen = set()


def check(name, extras):
    """Every requirement of `name` with these extras, and of what those require in turn."""
    key = (canonicalize_name(name), frozenset(extras))
    if key in seen:
        return
    seen.add(key)
    for line in distribution(name).requires or []:
        requirement = Requirement(line)
        # A requirement behind `extra == "x"` applies only when that extra was asked for.
        if requirement.marker and not any(requirement.marker.evaluate({"extra": extra}) for extra in (*extras, "")):
            continue
        try:
            installed = version(requirement.name)
        except PackageNotFoundError:
            failures.append(f"{name} needs {requirement}, which is not installed")
            continue
        if not requirement.specifier.contains(installed, prereleases=True):
            failures.append(f"{name} needs {requirement}, and {installed} is installed")
        check(requirement.name, requirement.extras)


# The mcp extra is what fastbrowse-mcp imports.
check("fastbrowse", {"mcp"})
print(f"{len(seen)} requirement sets checked")
if failures:
    raise SystemExit("\n".join(failures))
EOF

echo "fastbrowse $(pacman -Q fastbrowse | cut -d' ' -f2) builds, installs and runs"
