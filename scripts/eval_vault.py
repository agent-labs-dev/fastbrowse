"""Create the Bitwarden login items the live evals read with --bitwarden, from the practice sites' public logins.

Each item is saved for its task's start origin only (Host match), so the eval also exercises the origin check
that keeps a vault login off other sites. Items that already exist are left alone.

    export BW_SESSION=$(bw unlock --raw)
    uv run python scripts/eval_vault.py

This writes to your vault, so it is run by you, never by the eval.
"""

import json
import subprocess
import sys
from collections.abc import Mapping
from urllib.parse import urlsplit

from fastbrowse.adapters.bitwarden import UriMatch
from fastbrowse.evals.live_tasks import TASKS


def _bw(*args: str, stdin: str | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bw", *args, "--nointeraction"], input=stdin, capture_output=True, text=True, timeout=60, check=False
    )


def _item(name: str, origin: str, login: Mapping[str, str]) -> str:
    template = json.loads(_bw("get", "template", "item").stdout)
    template["name"] = name
    template["notes"] = "Public practice-site login used by fastbrowse's live evals."
    template["login"] = {
        "username": login.get("username"),
        "password": login.get("password"),
        "uris": [{"uri": origin, "match": UriMatch.HOST.value}],
    }
    return _bw("encode", stdin=json.dumps(template)).stdout.strip()


def main() -> int:
    wanted: dict[str, tuple[str, Mapping[str, str]]] = {}
    for task in TASKS:
        if task.bitwarden_item is not None:
            start = urlsplit(task.start)
            wanted[task.bitwarden_item] = (f"{start.scheme}://{start.netloc}", task.secrets)
    for name, (origin, login) in wanted.items():
        if _bw("get", "item", name).returncode == 0:
            print(f"exists  {name}")
            continue
        created = _bw("create", "item", _item(name, origin, login))
        if created.returncode != 0:
            # bw's stderr names a locked vault or a bad template; it never echoes the item.
            print(f"failed  {name}: {(created.stderr.strip() or 'no output').splitlines()[0]}", file=sys.stderr)
            return 1
        print(f"created {name} for {origin}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
