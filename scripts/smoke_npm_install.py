"""Install the SDK and one platform package as a user would, then start fastbrowse from them with no network.

    python scripts/smoke_npm_install.py pkg

`pkg` is the directory `scripts/npm_platform_package.py` wrote for the machine this runs on. The SDK is built and
packed from the checkout, so `npm ci` has to have run there. Both tarballs are installed into an empty project
with install scripts disabled, which is how pnpm and bun install by default. A script in that project then calls
`Fastbrowse.start()` with no binary named, so the one binary it can find is the one npm unpacked.

What this catches is a package that only works where it was built: a file npm left out of the tarball, an
executable bit lost on the way, a binary the SDK looks for under another path, a first start that fetches
something.

Standard library only, like the other smoke script, and for the same reason.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SDK = REPO / "packages" / "sdk"

START_SECONDS = 120.0

# Run in the project the packages were installed into. With `offline` it first shows that the network is gone,
# so a wrapper that stopped cutting it off cannot pass for one that does.
START = """
import { connect } from 'node:net';
import { Fastbrowse } from 'fastbrowse';

if (process.argv[2] === 'offline') {
  const reached = await new Promise(resolve => {
    const socket = connect({ host: 'registry.npmjs.org', port: 443 });
    socket.once('connect', () => resolve(true));
    socket.once('error', () => resolve(false));
  });
  if (reached) {
    console.error('the network was to be off, and registry.npmjs.org answered');
    process.exit(1);
  }
}
const fb = await Fastbrowse.start();
console.log(fb.fastbrowseVersion);
await fb.close();
"""


class Failed(Exception):
    """A step that did not hold, with what was seen instead."""


def offline() -> list[str] | None:
    """The command prefix that runs a program, and what it starts, with no network. None where there is none."""
    if sys.platform == "linux":
        # A network namespace of its own, holding no interface. Mapping the user to root is what lets an
        # unprivileged user make one.
        return ["unshare", "--map-root-user", "--net"]
    if sys.platform == "darwin":
        return ["sandbox-exec", "-n", "no-network"]
    # Windows has nothing of the kind that needs no firewall rule, so the start there runs with the network on.
    return None


def run(
    command: list[str], cwd: Path, env: dict[str, str] | None = None, timeout: float | None = None
) -> subprocess.CompletedProcess[str]:
    done = subprocess.run(command, cwd=cwd, env=env, timeout=timeout, capture_output=True, text=True)
    if done.returncode != 0:
        raise Failed(f"{' '.join(command)} exited with status {done.returncode}:\n{done.stdout}{done.stderr}")
    return done


def pack(npm: str, package: Path, into: Path) -> Path:
    """The tarball `npm publish` would upload for the package in this directory."""
    into.mkdir()
    run([npm, "pack", "--pack-destination", str(into)], cwd=package)
    (tarball,) = into.glob("*.tgz")
    return tarball


def check(package: Path) -> str:
    npm, node = shutil.which("npm"), shutil.which("node")
    if npm is None or node is None:
        raise Failed("npm and node have to be on the PATH")
    manifest = json.loads((package / "package.json").read_text(encoding="utf-8"))
    sdk_version = json.loads((SDK / "package.json").read_text(encoding="utf-8"))["version"]

    with tempfile.TemporaryDirectory() as temporary:
        work = Path(temporary)
        run([npm, "run", "build"], cwd=SDK)
        tarballs = [pack(npm, SDK, work / "sdk"), pack(npm, package, work / "platform")]

        project = work / "project"
        project.mkdir()
        (project / "package.json").write_text('{"private": true}\n', encoding="utf-8")
        run([npm, "install", "--ignore-scripts", "--no-audit", "--no-fund", *map(str, tarballs)], cwd=project)

        installed = project / "node_modules" / manifest["name"]
        missing = sorted(
            str(path.relative_to(package))
            for path in (package / "fastbrowse").rglob("*")
            if path.is_file() and not (installed / path.relative_to(package)).is_file()
        )
        if missing:
            raise Failed(f"the install lacks {len(missing)} files of the package, the first being {missing[0]}")
        executable = installed / "fastbrowse" / ("fastbrowse.exe" if os.name == "nt" else "fastbrowse")
        if os.name != "nt" and not os.access(executable, os.X_OK):
            raise Failed(f"{executable} came out of the install without its executable bit")

        (project / "start.mjs").write_text(START, encoding="utf-8")
        # Nothing but the installed platform package may supply the binary.
        environment = {name: value for name, value in os.environ.items() if name != "FASTBROWSE_BINARY"}
        prefix = offline()
        command = [node, "start.mjs"] if prefix is None else [*prefix, node, "start.mjs", "offline"]
        started = run(command, cwd=project, env=environment, timeout=START_SECONDS)

    version = started.stdout.strip()
    if not version == sdk_version == manifest["version"]:
        raise Failed(
            f"the binary is fastbrowse {version!r}, in {manifest['name']} {manifest['version']}, "
            f"started by the SDK at {sdk_version}: one release is one version"
        )
    network = "with the network on" if prefix is None else "with no network"
    return f"start: fastbrowse {version} from {manifest['name']}, installed without scripts, {network}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("package", type=Path, help="the platform package directory for this machine")
    args = parser.parse_args()
    try:
        print(check(args.package.resolve()))
    except (Failed, subprocess.TimeoutExpired) as exc:
        print(f"FAILED: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
