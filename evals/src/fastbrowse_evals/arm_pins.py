"""The competitor versions every comparison arm runs, checked against each one's latest release.

A comparison is only worth publishing against what a user installs today, so every pinned competitor is held at its
newest release. `adapters/arm-pins.json` is the one place the subprocess-installed competitors are pinned: the arm
adapters run in their own environments and read it next to themselves, and `live.py` builds its install specs and
recorded pins from it. The hosted Browser Use SDK is a dependency of this package, so its pin is `pyproject.toml`'s.

    python -m fastbrowse_evals.arm_pins check [--arms A B]   # exit 1 naming every pin behind its latest release
    python -m fastbrowse_evals.arm_pins update   # move every pin to its latest release
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from importlib.metadata import version as installed_version
from pathlib import Path
from typing import Any

import httpx

PINS_PATH = Path(__file__).with_name("adapters") / "arm-pins.json"
PROJECT = Path(__file__).resolve().parents[2]
SDK = "browser-use-sdk"
ARM_PINS = {
    "browser-use": (SDK,),
    "browser-use-ultrafast": (SDK,),
    "browser-use-oss": ("browser-use",),
    "browser-harness": ("browser-harness", "@openai/codex"),
    "cua-codex": ("cua-driver", "@openai/codex"),
}
"""The pins each comparison arm installs. The fastbrowse arms measure the campaign's own build and have none."""


def load() -> dict[str, dict[str, str]]:
    return json.loads(PINS_PATH.read_text())


def pinned(name: str) -> str:
    return load()[name]["version"]


def latest(http: httpx.Client, name: str, registry: str) -> str:
    """The newest release a user would install: PyPI's and npm's `latest`."""
    if registry == "pypi":
        return http.get(f"https://pypi.org/pypi/{name}/json").raise_for_status().json()["info"]["version"]
    if registry == "npm":
        return http.get(f"https://registry.npmjs.org/{name}/latest").raise_for_status().json()["version"]
    raise ValueError(f"{name}: unknown registry {registry!r}")


def stale(http: httpx.Client, arms: list[str] | None = None) -> list[dict[str, Any]]:
    """Every pin behind its latest release; with `arms`, only the pins those arms install."""
    entries = [{"name": name, **pin} for name, pin in load().items()]
    entries.append({"name": SDK, "registry": "pypi", "version": installed_version(SDK)})
    if arms is not None:
        wanted = {name for arm in arms for name in ARM_PINS.get(arm, ())}
        entries = [entry for entry in entries if entry["name"] in wanted]
    behind = []
    for entry in entries:
        newest = latest(http, entry["name"], entry["registry"])
        if newest != entry["version"]:
            behind.append({**entry, "latest": newest})
    return behind


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="fastbrowse_evals.arm_pins", description=__doc__.splitlines()[0])
    parser.add_argument("action", choices=("check", "update"))
    parser.add_argument("--arms", nargs="+", help="check only the pins these arms install")
    args = parser.parse_args(argv)
    with httpx.Client(timeout=30, follow_redirects=True) as http:
        behind = stale(http, args.arms)
    for entry in behind:
        print(f"{entry['name']} ({entry['registry']}): pinned {entry['version']}, latest {entry['latest']}")
    if args.action == "check":
        if behind:
            print(
                "Run `python -m fastbrowse_evals.arm_pins update` and land the bump before comparing.", file=sys.stderr
            )
        return 1 if behind else 0
    pins = load()
    for entry in behind:
        if entry["name"] == SDK:
            subprocess.run(["uv", "add", "--project", str(PROJECT), f"{SDK}=={entry['latest']}"], check=True)
        else:
            pins[entry["name"]]["version"] = entry["latest"]
    PINS_PATH.write_text(json.dumps(pins, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
