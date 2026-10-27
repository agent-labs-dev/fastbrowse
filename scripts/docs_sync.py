"""Fail when the docs describe something the code does not have.

The README, `docs`, the skill and the SDK page are read by people and built into fastbrowse.ai, so a
sentence that names a flag, a setting, a status, an exit code or a file the repository no longer has is a
stale product claim the site publishes too. Prose checks elsewhere catch punctuation and wording; this
one catches a doc that has drifted from the code.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Product copy. ADRs and retrieved evidence are records of a decision or a run, so they are not updated
# to match today's code and are not checked.
COPY = (
    "README.md",
    "CHANGELOG.md",
    "AGENTS.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "docs/benchmarks.md",
    "docs/data-sources.md",
    "docs/design.md",
    "docs/evals.md",
    "docs/jev.md",
    "docs/native-typesafe.md",
    "docs/skill.md",
    "docs/troubleshooting.md",
    "packages/sdk/README.md",
    "skills/fastbrowse/SKILL.md",
    "skills/fastbrowse/references/browser-access.md",
)

CODE = ("src", "crates")

FLAG = re.compile(r"--[a-z0-9][a-z0-9-]*")
ENV = re.compile(r"FASTBROWSE_[A-Z0-9_]+")
LINK = re.compile(r"\]\(([^)\s]+)")
STATUS_ROW = re.compile(r"^\| `([a-z_]+)` \| (\d+) \|", re.MULTILINE)
TROUBLE_HEADING = re.compile(r"^## ([a-z_]+) \(exit (\d+)\)", re.MULTILINE)
STATUS_MEMBER = re.compile(r'^\s+([A-Z_]+) = "([a-z_]+)"', re.MULTILINE)
EXIT_ENTRY = re.compile(r"Status\.([A-Z_]+): (\d+),")


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def code_text(root: Path) -> str:
    parts = []
    for name in CODE:
        for path in (root / name).rglob("*"):
            if path.suffix in {".py", ".rs", ".toml", ".json"} and path.is_file():
                parts.append(read(path))
    return "\n".join(parts)


def repo_text(root: Path) -> str:
    parts = []
    for path in root.rglob("*"):
        if path.is_file() and path.suffix in {".py", ".rs", ".toml", ".md", ".sh", ".yml"}:
            parts.append(read(path))
    return "\n".join(parts)


def statuses(root: Path) -> dict[str, str]:
    """Each status's wire name, keyed by its enum member."""
    models = read(root / "src/fastbrowse/models.py")
    body = models.split("class Status(StrEnum):", 1)[1].split("\nclass ", 1)[0]
    return dict(STATUS_MEMBER.findall(body))


def exit_codes(root: Path) -> dict[str, str]:
    """Each status's exit code, keyed by its enum member."""
    return dict(EXIT_ENTRY.findall(read(root / "src/fastbrowse/cli.py")))


def check_flags(copy: dict[str, str], code: str) -> list[str]:
    """A flag in a README table is a flag the command line or the MCP server parses."""
    known = set(FLAG.findall(code))
    problems = []
    for name, text in copy.items():
        for line in text.splitlines():
            if not line.startswith("| `--"):
                continue
            for flag in FLAG.findall(line):
                if flag not in known:
                    problems.append(f"{name}: `{flag}` is documented but no parser defines it")
    return problems


def check_env(copy: dict[str, str], everything: str) -> list[str]:
    """A documented setting is one the package reads."""
    known = set(ENV.findall(everything))
    problems = []
    for name, text in copy.items():
        for setting in sorted(set(ENV.findall(text))):
            if setting not in known:
                problems.append(f"{name}: {setting} is documented but nothing reads it")
    return problems


def check_statuses(root: Path, copy: dict[str, str]) -> list[str]:
    """The status table in the README and the troubleshooting headings name every status, with its exit code."""
    names = set(statuses(root).values())
    codes = exit_codes(root)
    problems = []

    readme = copy.get("README.md", "")
    rows = {name: code for name, code in STATUS_ROW.findall(readme)}
    for member, name in statuses(root).items():
        if name not in rows:
            problems.append(f"README.md: the status table has no row for `{name}`")
        elif member in codes and rows[name] != codes[member]:
            problems.append(f"README.md: `{name}` exits {rows[name]}, but the CLI exits {codes[member]}")
    for name in rows:
        if name not in names:
            problems.append(f"README.md: the status table documents `{name}`, which is not a status")

    headings = dict(TROUBLE_HEADING.findall(copy.get("docs/troubleshooting.md", "")))
    for member, name in statuses(root).items():
        # A run that finished is nothing to troubleshoot.
        if name == "complete":
            continue
        if name not in headings:
            problems.append(f"docs/troubleshooting.md: no section for `{name}`")
        elif member in codes and headings[name] != codes[member]:
            problems.append(
                f"docs/troubleshooting.md: `{name}` says exit {headings[name]}, but the CLI exits {codes[member]}"
            )
    return problems


def check_links(root: Path, copy: dict[str, str]) -> list[str]:
    """A relative link points at a file in this repository."""
    problems = []
    for name, text in copy.items():
        base = (root / name).parent
        for target in LINK.findall(text):
            if target.startswith(("http://", "https://", "#", "mailto:")):
                continue
            path = target.split("#")[0]
            if not path:
                continue
            if not (base / path).exists():
                problems.append(f"{name}: `{target}` does not exist")
    return problems


def check_changelog(root: Path, copy: dict[str, str]) -> list[str]:
    """A release publishes its changelog entry, so the version in the manifest has one."""
    version = re.search(r'^version = "([^"]+)"', read(root / "pyproject.toml"), re.MULTILINE)
    if version is None:
        return ["pyproject.toml: no version"]
    heading = f"## [{version.group(1)}]"
    if heading not in copy.get("CHANGELOG.md", ""):
        return [f"CHANGELOG.md: no `{heading}` entry for the version pyproject.toml declares"]
    return []


def check(root: Path = ROOT) -> list[str]:
    copy = {name: read(root / name) for name in COPY if (root / name).exists()}
    missing = [name for name in COPY if not (root / name).exists()]
    problems = [f"{name}: listed as product copy but not in the repository" for name in missing]
    problems += check_links(root, copy)
    problems += check_flags(copy, code_text(root))
    problems += check_env(copy, repo_text(root))
    problems += check_statuses(root, copy)
    problems += check_changelog(root, copy)
    return problems


def main() -> int:
    problems = check()
    if problems:
        print("The docs disagree with the code:")
        for problem in problems:
            print(f"  {problem}")
        return 1
    print(f"{len(COPY)} product files match the code")
    return 0


if __name__ == "__main__":
    sys.exit(main())
