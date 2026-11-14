"""Refuse to publish a fastbrowse whose extension the Chrome Web Store has not approved yet.

The extension and the CLI reach users on two channels that move at different speeds: the Store reviews an upload,
while a PyPI release follows a tag within the hour. A release whose runs need methods the approved package does not
relay is broken for every new Store install on its first run (issue 1063), so the release pipeline asks this first
and stops with what to do about it.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

COMPAT = Path(__file__).resolve().parents[1] / "crates" / "fastbrowse-browser" / "extension-store" / "compat.json"


def blocking(compat: dict) -> str | None:
    """Why this tree cannot be released yet, or `None` when the Store package satisfies it."""
    store = compat.get("store", {})
    missing = store.get("missing") or []
    if not missing:
        return None
    needs = store.get("needs", "a newer version")
    served = store.get("version", "an older version")
    return (
        f"the Chrome Web Store serves extension {served}, which does not relay {', '.join(missing)}; a release now "
        f"would refuse every run from a Store install. Publish extension {needs} and install it from the Store, then "
        f"replace extension-store/published/{served} with the approved package's files and empty store.missing in "
        f"extension-store/compat.json. Tracking: {store.get('issue', 'no issue recorded')}"
    )


def main() -> int:
    problem = blocking(json.loads(COMPAT.read_text(encoding="utf-8")))
    if problem:
        print(f"extension store gate: {problem}", file=sys.stderr)
        return 1
    print("extension store gate: the Store package satisfies this release")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
