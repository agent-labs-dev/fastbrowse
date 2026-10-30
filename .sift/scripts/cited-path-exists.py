#!/usr/bin/env python3
# sift-scope: changed
# sift-fix: Point the comment at the file that holds the code now, or drop a citation of a removed file.
"""Report code comments that cite a repository path which no longer exists.

A path resolves when it exists from the repository root or from any directory
above the citing file (crate-relative paths such as `tests/agent.rs`).
"""

import os
import re
import sys
from pathlib import Path

CODE = {".rs", ".py", ".ts", ".js", ".go", ".cs", ".java"}
PATH = re.compile(
    r"(?<![\w./-])((?:src|tests|crates|scripts|packages|bench|docs|protocol)/[\w./-]*\w\.(?:py|rs|ts|js|json|md|go|toml|ya?ml))(?![\w/])"
)
COMMENT = re.compile(r"^\s*(?://|#|/\*|\*)|\s(?://|#)\s")


def resolves(root: Path, citing: str, cited: str) -> bool:
    here = (root / citing).parent
    while True:
        if (here / cited).exists():
            return True
        if here == root:
            return False
        here = here.parent


def main() -> int:
    root = Path(os.environ["SIFT_ROOT"])
    rule = os.environ["SIFT_RULE"]
    if os.environ.get("SIFT_CHANGED"):
        paths = []
        for line in Path(os.environ["SIFT_CHANGED"]).read_text().splitlines():
            parts = line.split("\t")
            if parts[0] in ("A", "M"):
                paths.append(parts[1])
            elif parts[0] == "R":
                paths.append(parts[2])
    else:
        paths = Path(os.environ["SIFT_FILES"]).read_text().splitlines()
    found = 0
    for path in sorted(set(paths)):
        if path.startswith(".sift/") or Path(path).suffix not in CODE or not (root / path).is_file():
            continue
        text = (root / path).read_text(errors="replace")
        for number, line in enumerate(text.splitlines(), 1):
            if not COMMENT.search(line):
                continue
            for cited in PATH.findall(line):
                if not resolves(root, path, cited):
                    print(f"{path}:{number}: {rule} cites {cited}, which does not exist")
                    found += 1
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
