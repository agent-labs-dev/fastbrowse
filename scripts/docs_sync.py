"""Fail when the docs describe something the code does not have.

The README, `docs`, the skill and the SDK page are read by people and built into fastbrowse.ai, so a
sentence that names a flag, a setting, a status, an exit code or a file the repository no longer has is a
stale product claim the site publishes too. Prose checks elsewhere catch punctuation and wording; this
one catches a doc that has drifted from the code.
"""

from __future__ import annotations

import hashlib
import json
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

MEDIA_RECORD = "docs/assets/media.json"

CODE = ("src", "crates")
# Installed or built trees: what a dependency reads is not a setting this package has.
NOT_OURS = {".git", ".venv", "node_modules", "target", "dist", "build"}

FLAG = re.compile(r"--[a-z0-9][a-z0-9-]*")
# A flag exists when a parser declares it: argparse's `add_argument("--x", ...)` or the Rust CLI's
# `Opt::flag("--x")`. A flag named only in a docstring or an error message does not count.
FLAG_DECLARATION = re.compile(r'(?:add_argument\(\s*(?:"-[a-z]",\s*)?|Opt::[a-z]+\(\s*)"(--[a-z0-9][a-z0-9-]*)"')
ENV = re.compile(r"FASTBROWSE_[A-Z0-9_]+")
LINK = re.compile(r"\]\(([^)\s]+)")
STATUS_ROW = re.compile(r"^\| `([a-z_]+)` \| (\d+) \|", re.MULTILINE)
MEDIA = re.compile(r'!\[[^\]]*\]\(([^)\s]+)|<(?:img|video|source)[^>]*\bsrc(?:set)?="([^"]+)"')
TROUBLE_HEADING = re.compile(r"^## ([a-z_]+) \(exit (\d+)\)", re.MULTILINE)
STATUS_MEMBER = re.compile(r'^\s+([A-Z_]+) = "([a-z_]+)"', re.MULTILINE)
EXIT_ENTRY = re.compile(r"Status::([A-Za-z]+) => (\d+),")
CLI = "crates/fastbrowse-cli/src/cli.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def code_text(root: Path) -> str:
    parts = []
    for name in CODE:
        for path in (root / name).rglob("*"):
            if path.suffix in {".py", ".rs", ".toml", ".json"} and path.is_file():
                parts.append(read(path))
    return "\n".join(parts)


def implementation_text(root: Path) -> str:
    """Everything that could read a setting: code, scripts, configuration and workflows, but no prose."""
    parts = []
    for path in root.rglob("*"):
        if NOT_OURS.intersection(path.relative_to(root).parts):
            continue
        if path.is_file() and path.suffix in {".py", ".rs", ".toml", ".sh", ".yml", ".ts", ".js"}:
            parts.append(read(path))
    return "\n".join(parts)


def statuses(root: Path) -> dict[str, str]:
    """Each status's wire name, keyed by its enum member."""
    models = read(root / "src/fastbrowse/models.py")
    body = models.split("class Status(StrEnum):", 1)[1].split("\nclass ", 1)[0]
    return dict(STATUS_MEMBER.findall(body))


def exit_codes(root: Path) -> dict[str, str]:
    """Each status's exit code, keyed by its enum member, as the CLI's `exit_code` maps it."""
    body = read(root / CLI).split("pub fn exit_code(", 1)[1].split("\n}", 1)[0]
    return {re.sub(r"(?<!^)(?=[A-Z])", "_", variant).upper(): code for variant, code in EXIT_ENTRY.findall(body)}


def check_flags(copy: dict[str, str], code: str) -> list[str]:
    """A flag in a README table is a flag the command line or the MCP server parses."""
    known = set(FLAG_DECLARATION.findall(code))
    problems = []
    for name, text in copy.items():
        for line in text.splitlines():
            if not line.startswith("| `--"):
                continue
            for flag in FLAG.findall(line):
                if flag not in known:
                    problems.append(f"{name}: `{flag}` is documented but no parser defines it")
    return problems


def check_env(copy: dict[str, str], implementation: str) -> list[str]:
    """A documented setting is one the package reads."""
    known = set(ENV.findall(implementation))
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
    problems = [
        f"{CLI}: `{name}` has no exit code in `exit_code`"
        for member, name in statuses(root).items()
        if member not in codes
    ]

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


def media_references(sources: dict[str, str]) -> dict[str, list[str]]:
    """Each local media file the copy shows, with the files that show it."""
    found: dict[str, list[str]] = {}
    for name, text in sources.items():
        for markdown, html in MEDIA.findall(text):
            target = markdown or html
            if not target or target.startswith(("http://", "https://", "data:")):
                continue
            for part in target.split(","):
                path = part.strip().split(" ")[0].split("#")[0]
                if path:
                    found.setdefault(path, []).append(name)
    return found


def surface(root: Path) -> str:
    """A fingerprint of the user-visible command line: the flags, statuses and exit codes a recording shows."""
    code = code_text(root)
    parts = sorted(set(FLAG.findall(code)))
    parts += sorted(f"{member}={name}" for member, name in statuses(root).items())
    parts += sorted(f"{member}:{code}" for member, code in exit_codes(root).items())
    return hashlib.sha256("\n".join(parts).encode()).hexdigest()


def check_media(root: Path, copy: dict[str, str]) -> list[str]:
    """Every asset the copy shows exists, is not empty, and was recorded against the surface we ship now."""
    problems = []
    referenced = media_references(copy)
    for path, sources in sorted(referenced.items()):
        asset = root / path
        if not asset.exists():
            problems.append(f"{', '.join(sorted(set(sources)))}: `{path}` is shown but not in the repository")
        elif asset.stat().st_size == 0:
            problems.append(f"{path}: the file is empty")

    record = root / MEDIA_RECORD
    if referenced and not record.exists():
        problems.append(f"{MEDIA_RECORD}: media is shown in the copy, but no record says what it shows")
        return problems

    recorded = json.loads(read(record)).get("assets", {}) if record.exists() else {}
    now = surface(root)
    for path in sorted(referenced):
        entry = recorded.get(path)
        if entry is None:
            problems.append(f"{MEDIA_RECORD}: no record for `{path}`")
            continue
        if entry.get("surface") != now:
            problems.append(
                f"{MEDIA_RECORD}: `{path}` was recorded against a different command line or status set, "
                "so it shows a product we no longer ship. Re-record it and run --write"
            )
    for path in sorted(set(recorded) - set(referenced)):
        problems.append(f"{MEDIA_RECORD}: `{path}` is recorded but no copy shows it")
    return problems


def check(root: Path = ROOT) -> list[str]:
    copy = {name: read(root / name) for name in COPY if (root / name).exists()}
    missing = [name for name in COPY if not (root / name).exists()]
    problems = [f"{name}: listed as product copy but not in the repository" for name in missing]
    problems += check_links(root, copy)
    problems += check_flags(copy, code_text(root))
    # The changelog keeps old entries verbatim, so it still names settings a release removed.
    current = {name: text for name, text in copy.items() if name != "CHANGELOG.md"}
    problems += check_env(current, implementation_text(root))
    problems += check_statuses(root, copy)
    problems += check_changelog(root, copy)
    problems += check_media(root, copy)
    return problems


def write_record(root: Path = ROOT) -> None:
    """Record what each shown asset is, and the surface it was recorded against."""
    copy = {name: read(root / name) for name in COPY if (root / name).exists()}
    record = root / MEDIA_RECORD
    existing = json.loads(read(record)).get("assets", {}) if record.exists() else {}
    assets = {}
    for path in sorted(media_references(copy)):
        entry = existing.get(path, {})
        entry["surface"] = surface(root)
        assets[path] = entry
    record.parent.mkdir(parents=True, exist_ok=True)
    record.write_text(json.dumps({"assets": assets}, indent=2) + "\n", encoding="utf-8")
    print(f"{MEDIA_RECORD}: {len(assets)} assets recorded")


def main() -> int:
    if "--write" in sys.argv:
        write_record()
        return 0
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
