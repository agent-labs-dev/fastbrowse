#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Agent Labs
# SPDX-License-Identifier: MIT
"""Check repository agent instructions and resolve declared standards without running project code."""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Iterator
from dataclasses import dataclass
from itertools import pairwise
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlsplit

sys.dont_write_bytecode = True

VERSION = "0.3.0"
MAX_BYTES = 32 * 1024
NOTE_BYTES = 12 * 1024
MAX_STANDARD_FILES = 1000
MAX_STANDARD_BYTES = 16 * 1024 * 1024
SKIP_DIRS = {".git", "node_modules", "vendor", "dist", "build", ".venv", "target", "__pycache__"}
PATH_EXTENSIONS = set(
    "py pyi js jsx ts tsx mjs cjs mts cts lua luau sh bash zsh rb go rs c h cc cpp hpp cs java kt "  # noqa: SIM905 (a word list reads best as one string)
    "swift php ex exs sql html css scss vue svelte md markdown mdx rst txt json jsonc yaml yml toml "
    "ini cfg conf xml svg png jpg jpeg gif webp pdf csv tsv lock toc rockspec wasm zip gz".split()
)
BUILTINS = set(
    ": . [ alias bg bind break builtin caller cd command compgen complete continue declare dirs "  # noqa: SIM905 (a word list reads best as one string)
    "disown echo enable eval exec exit export false fc fg getopts hash help history jobs kill let "
    "local logout mapfile popd printf pushd pwd read readarray readonly return set shift shopt source "
    "test times trap true type typeset ulimit umask unalias unset wait which".split()
)
CONTROL_WORDS = set(
    "if then else elif fi for while until do done case esac select function { } !".split()  # noqa: SIM905 (a word list reads best as one string)
)
SHELL_TAGS = {"", "sh", "bash", "shell", "console", "zsh"}
FENCE = re.compile(r" {0,3}(`{3,}|~{3,})(.*)")
HEADING = re.compile(r" {0,3}(#{1,6})\s+(.+?)\s*#*\s*$")
INLINE_CODE = re.compile(r"(?<!`)(`+)([^\n]*?[^`])\1(?!`)")
LINK = re.compile(
    r"!?\[([^\]\n]*)\]\(\s*(<[^>\n]*>|(?:[^\s()\\]|\\.|\([^()]*\))+)(?:\s+['\"][^\n]*?['\"])?\s*\)"
)
REFERENCE = re.compile(r" {0,3}\[([^\]]+)\]:\s*(<[^>]+>|\S+)")
SCHEME = re.compile(r"^@?[a-zA-Z][a-zA-Z0-9+.-]*:")
PLACEHOLDER = re.compile(r"<[^>\n]+>")
PACK_NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
GITHUB_URL = re.compile(r"https://github\.com/[^\s<>`\"'()\[\]]+")
SKILL_PATH = re.compile(r"(?:^|/)(?:\.agents|\.claude)/skills/([^/]+)(?:/(.*))?$")
Issue = dict[str, Any]


def issue(code: str, file: str, line: int, message: str, severity: str = "error") -> Issue:
    return {"code": code, "file": file, "line": line, "message": message, "severity": severity}


def git(root: Path, *args: str) -> str | None:
    try:
        result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, check=False)
    except OSError:
        return None
    return os.fsdecode(result.stdout) if result.returncode == 0 else None


def default_root() -> Path:
    result = git(Path.cwd(), "rev-parse", "--show-toplevel")
    return Path(result.rstrip("\n")) if result is not None else Path.cwd()


def enumerate_files(root: Path) -> list[str]:
    result = git(root, "ls-files", "-co", "--exclude-standard", "-z", "--", ".")
    if result is not None:
        names = set(filter(None, result.split("\0")))
    else:
        names = set()
        for base, dirs, files in os.walk(root, onerror=raise_io):
            dirs[:] = sorted(name for name in dirs if name not in SKIP_DIRS)
            names.update((Path(base) / name).relative_to(root).as_posix() for name in files)
    return sorted(
        name
        for name in names
        if Path(name).name == "AGENTS.md" and not SKIP_DIRS.intersection(Path(name).parts)
    )


def raise_io(error: OSError) -> None:
    raise error


@dataclass
class Markdown:
    prose: list[str]
    blocks: list[tuple[int, str, list[tuple[int, str]]]]

    def sections(self) -> Iterator[tuple[int, str, int]]:
        """Yield H2 sections, ending at the next H1/H2, with zero-based bounds."""
        headings = [(i, match) for i, line in enumerate(self.prose) if (match := HEADING.fullmatch(line))]
        for index, (start, heading) in enumerate(headings):
            if len(heading[1]) == 2:
                end = next((i for i, h in headings[index + 1 :] if len(h[1]) <= 2), len(self.prose))
                yield start, heading[2], end


def markdown(content: str) -> Markdown:
    content = re.sub(r"<!--.*?(?:-->|\Z)", lambda m: "\n" * m[0].count("\n"), content, flags=re.S)
    prose: list[str] = []
    blocks: list[tuple[int, str, list[tuple[int, str]]]] = []
    fence = ""
    for number, line in enumerate(content.splitlines(), 1):
        match = FENCE.fullmatch(line)
        if fence:
            if match and match[1][0] == fence[0] and len(match[1]) >= len(fence) and not match[2].strip():
                fence = ""
            else:
                blocks[-1][2].append((number, line))
            prose.append("")
        elif match:
            fence = match[1]
            tag = match[2].strip().split()
            blocks.append((number, tag[0].lower() if tag else "", []))
            prose.append("")
        else:
            prose.append(line)
    return Markdown(prose, blocks)


def has_summary(document: Markdown) -> bool:
    prose = document.prose
    first_section = next(document.sections(), None)
    if first_section and re.fullmatch(
        r"(?:project\s+)?(?:overview|summary|intro(?:duction)?|about)", first_section[1], re.I
    ):
        start, _, end = first_section
        prose = prose[:start] + prose[start + 1 : end]
    end = next((i for i, line in enumerate(prose) if re.match(r" {0,3}##\s", line)), len(prose))
    start = next((i + 1 for i, line in enumerate(prose[:end]) if re.match(r" {0,3}#\s", line)), 0)
    for paragraph in re.split(r"\n\s*\n", "\n".join(prose[start:end])):
        first = paragraph.strip()
        if first and not re.match(r"(?:#|[-+*]\s|[>|]|\d+[.)]\s|\[[^]]+\]:|<|```|~~~)", first):
            return True
    return False


def links(document: Markdown) -> Iterator[tuple[int, str]]:
    """Literal inline destinations and reference definitions (including unused definitions)."""
    for number, line in enumerate(document.prose, 1):
        clean = INLINE_CODE.sub("", line)
        if definition := REFERENCE.match(clean):
            yield number, definition[2].strip("<>")
        else:
            for match in LINK.finditer(clean):
                yield number, match[2].strip("<>")


def relative_link(target: str) -> str | None:
    if not target or target.startswith(("#", "/")):
        return None
    value = prose_path(unquote(re.sub(r"\\([() ])", r"\1", target.split("#", 1)[0])))
    return None if SCHEME.match(value) else value or None


def prose_path(value: str) -> str:
    return re.sub(r"::[A-Za-z_][\w.:]*$|:\d+(?::\d+)?(?:-\d+(?::\d+)?)?$", "", value)


def expand_braces(value: str) -> Iterator[str]:
    match = re.search(r"\{([^{}]*,[^{}]*)\}", value)
    if match is None:
        yield value
    else:
        for part in match[1].split(","):
            yield from expand_braces(value[: match.start()] + part + value[match.end() :])


def path_like(value: str, *, command: bool = False) -> bool:
    if (
        not value
        or any(char.isspace() for char in value)
        or value.startswith(("-", "~", "$", "/"))
        or SCHEME.match(value)
        or PLACEHOLDER.search(value)
        or re.search(r"\{[^{}]*(?:\.\.\.|\u2026)[^{}]*\}", value)
        or any(char in value for char in "`$=|;<>\\")
        or re.fullmatch(r"v?\d+(?:\.\d+)+(?:[-+][\w.-]+)?", value)
        or (not command and re.fullmatch(r"[A-Za-z_]\w*(?:\.[A-Za-z_]\w*){2,}", value))
    ):
        return False
    extension = Path(value.rstrip("/")).suffix.lstrip(".").lower()
    if "/" not in value:
        return extension in PATH_EXTENSIONS
    # Ambiguous owner/repo names are not actionable prose paths; explicit paths/globs are.
    return bool(
        command
        or value.count("/") != 1
        or value.startswith(".")
        or value.endswith("/")
        or extension in PATH_EXTENSIONS
        or glob.has_magic(value)
    )


def existing_path(value: str, directory: Path, root: Path, *context: Path) -> Path | None:
    for base in dict.fromkeys((directory, root, *context)):
        candidate = base / value
        if glob.has_magic(value):
            for match in glob.iglob(str(candidate), recursive=True):
                if Path(match).exists():
                    return Path(match)
        elif candidate.exists():
            return candidate
    return None


def scalar(value: str) -> str:
    """Read a literal YAML string scalar; collections, aliases and implicit null/bools are not strings."""
    value = value.strip()
    quoted = re.fullmatch(r"""("(?:\\.|[^"\\])*"|'(?:''|[^'])*')\s*(?:#.*)?""", value)
    value = quoted[1] if quoted else re.sub(r"\s+#.*$", "", value).strip()
    if value.startswith('"'):
        try:
            decoded = json.loads(value)
        except ValueError:
            return ""
        return decoded.strip() if isinstance(decoded, str) else ""
    if value.startswith("'"):
        return value[1:-1].replace("''", "'").strip() if value.endswith("'") else ""
    if not value or value[0] in "[{*&!#" or value.lower() in {"null", "~", "true", "false"}:
        return ""
    return "" if re.fullmatch(r"[-+]?\d+(?:\.\d+)?", value) else value


def frontmatter_error(path: Path) -> str | None:  # noqa: PLR0911 (independent validation failures)
    if not path.is_file():
        return "missing SKILL.md"
    lines = path.read_text(encoding="utf-8").splitlines()
    if not lines or lines[0].strip() != "---":
        return "missing YAML frontmatter"
    end = next((i for i, line in enumerate(lines[1:], 1) if line.strip() in {"---", "..."}), None)
    if end is None:
        return "unclosed YAML frontmatter"
    fields = {}
    for index, line in enumerate(lines[1:end], 1):
        match = re.match(r"^(name|description):\s*(.*)$", line)
        if not match:
            continue
        if match[1] in fields:
            return f"duplicate {match[1]} in frontmatter"
        value = scalar(match[2])
        if re.fullmatch(r"[|>][+-]?", value):
            body = []
            for continuation in lines[index + 1 : end]:
                if continuation and not continuation[0].isspace():
                    break
                body.append(continuation.strip())
            value = " ".join(body).strip()
        fields[match[1]] = value
    if fields.get("name") != path.parent.name:
        return "frontmatter name must equal the skill directory name"
    if not fields.get("description"):
        return "frontmatter description must be a non-empty string"
    return None


def skill_error(value: str, directory: Path, root: Path, *, link: bool) -> str | None:
    match = SKILL_PATH.search(value.rstrip("/"))
    if not match or (match[2] != "SKILL.md" and not (link and not match[2])):
        return None
    target = existing_path(value, directory, directory if link else root) or directory / value
    path = target if match[2] else target / "SKILL.md"
    return frontmatter_error(path)


def inline_context(prefix: str, value: str, directory: Path, root: Path) -> list[Path]:
    """Use only directories named or `cd`-ed into earlier on this line, never a repo-wide search."""
    context = []
    for _, target in links(Markdown([prefix], [])):
        if (path := relative_link(target)) and (directory / path).exists():
            candidate = directory / path
            context.append(candidate if candidate.is_dir() else candidate.parent)
    for match in INLINE_CODE.finditer(LINK.sub("", prefix)):
        path = prose_path(re.sub(r"^cd\s+(\S+)(?:\s*&&.*)?$", r"\1", match[2].strip()))
        if (
            path_like(path.rstrip("/") + "/", command=True)
            and (candidate := existing_path(path, directory, root))
            and candidate.is_dir()
        ):
            context.append(candidate)
    # A linked docs/topic/guide.md can introduce both details.md and topic/details.md.
    return context + [base.parent for base in context if value.startswith(base.name + "/")]


def nonlocal_reference(prefix: str, value: str) -> bool:
    if re.search(
        r"\b(?:no(?: new)?|(?:never|do not|don't) (?:add|create|introduce|use))\s*$",
        prefix.replace("*", ""),
        re.I,
    ):
        return True
    return any(
        value.startswith(match[1] + "/")
        for match in re.finditer(r"`([^`\s/]+)`\s+repo(?:sitory)?\b", prefix, re.I)
    )


def reference_issues(root: Path, file: str, document: Markdown) -> list[Issue]:
    result = []
    directory = (root / file).parent
    for number, target in links(document):
        value = relative_link(target)
        if value is None:
            continue
        if error := skill_error(value, directory, root, link=True):
            result.append(issue("dead-skill", file, number, f"{value}: {error}"))
        elif not (directory / value).exists():
            result.append(issue("dead-link", file, number, f"link target does not exist: {value}"))
    for number, line in enumerate(document.prose, 1):
        # A link's label is display text; its destination is checked above.
        clean = LINK.sub(lambda m: " " * len(m[0]), line)
        for match in INLINE_CODE.finditer(clean):
            value = prose_path(match[2].strip())
            prefix = line[: match.start()]
            if not path_like(value, command=True) or nonlocal_reference(prefix, value):
                continue
            context = inline_context(prefix, value, directory, root)
            if not path_like(value):
                parent = existing_path(value.split("/", 1)[0], directory, root, *context)
                if parent is None or not parent.is_dir():
                    continue
            for path in dict.fromkeys(expand_braces(value)):
                if error := skill_error(path, directory, root, link=False):
                    result.append(issue("dead-skill", file, number, f"{path}: {error}"))
                elif existing_path(path, directory, root, *context) is None:
                    result.append(issue("dead-path", file, number, f"path does not exist: {path}"))
    return result


def nearest(directory: Path, root: Path, names: tuple[str, ...]) -> Path | None:
    for base in (directory, *directory.parents):
        if not base.is_relative_to(root):
            break
        for name in names:
            if (base / name).is_file():
                return base / name
    return None


def available_tool(name: str, directory: Path, root: Path) -> bool:
    if shutil.which(name):
        return True
    for base in (directory, *directory.parents):
        if not base.is_relative_to(root):
            break
        for bindir in (base / ".venv/bin", base / ".venv/Scripts"):
            for filename in (name, name + ".exe"):
                candidate = bindir / filename
                if candidate.is_file() and os.access(candidate, os.X_OK):
                    return True
    return False


def command_segments(line: str) -> list[list[str]]:  # noqa: PLR0912 (quote/escape lexer states)
    """Split unquoted shell operators/comments; never interpret substitutions or execute commands."""
    line = re.sub(r"^\s*\$ ", "", line)
    if PLACEHOLDER.search(line):
        return []
    chunks, current = [], []
    quote = ""
    escaped = False
    for char in line:
        if escaped:
            current.append(char)
            escaped = False
        elif char == "\\" and quote != "'":
            current.append(char)
            escaped = True
        elif char == quote:
            quote = ""
            current.append(char)
        elif not quote and char in "'\"":
            quote = char
            current.append(char)
        elif not quote and char == "#" and (not current or current[-1].isspace()):
            break
        elif not quote and char == "&" and current and current[-1] in "<>":
            current.append(char)
        elif not quote and char in ";|&":
            chunks.append("".join(current))
            current = []
        else:
            current.append(char)
    chunks.append("".join(current))
    result = []
    for chunk in chunks:
        try:
            words = shlex.split(chunk)
        except ValueError:
            continue
        words = without_redirections(words)
        while words and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", words[0]):
            words.pop(0)
        if words and words[0] not in CONTROL_WORDS:
            result.append(words)
    return result


def without_redirections(words: list[str]) -> list[str]:
    """Drop `> out.log`, `2>&1`, `<in` and similar: they name streams, not targets or scripts."""
    result: list[str] = []
    skip = False
    for word in words:
        if skip:
            skip = False
        elif re.fullmatch(r"\d*(?:&?>>?|<<?<?)", word):
            skip = True
        elif not re.fullmatch(r"\d*(?:&?>>?|<<?<?)\S+", word):
            result.append(word)
    return result


def python_tools(path: Path) -> tuple[set[str], set[str]]:
    """Extract literal script keys and dependency names without requiring Python 3.11's tomllib."""
    content = path.read_text(encoding="utf-8")
    # Mask comments while preserving quoted strings (including markers such as '#').
    content = re.sub(r"""("(?:\\.|[^"\\])*"|'[^']*')|#[^\n]*""", lambda m: m[1] or "", content)
    sections = re.split(r"(?m)^\s*\[([^\]\n]+)\]\s*$", content)
    scripts: set[str] = set()
    dependencies: set[str] = set()
    for table, section in zip(sections[1::2], sections[2::2], strict=True):
        body = section
        if table in {"project.scripts", "project.gui-scripts", "tool.poetry.scripts"}:
            scripts.update(re.findall(r"""(?m)^\s*["']?([\w.-]+)["']?\s*=""", body))
        if table == "project":
            match = re.search(
                r"""(?ms)^\s*dependencies\s*=\s*\[((?:"(?:\\.|[^"\\])*"|'[^']*'|[^\]"'])*)\]""", body
            )
            body = match[0] if match else ""
        elif table == "tool.uv":
            match = re.search(
                r"""(?ms)^\s*dev-dependencies\s*=\s*\[((?:"(?:\\.|[^"\\])*"|'[^']*'|[^\]"'])*)\]""", body
            )
            body = match[0] if match else ""
        elif table.endswith("dependencies") and table.startswith("tool.poetry."):
            dependencies.update(re.findall(r"""(?m)^\s*["']?([\w.-]+)["']?\s*=""", body))
            continue
        elif table not in {"project.optional-dependencies", "dependency-groups"}:
            continue
        dependencies.update(dependency_names(body))
    return scripts, {re.sub(r"[-_.]+", "-", name).lower() for name in dependencies}


def dependency_names(body: str) -> set[str]:
    """Only strings inside arrays are dependencies; quoted keys/include-group objects are not."""
    names = set()
    depth = objects = 0
    for token in re.finditer(r"""("(?:\\.|[^"\\])*"|'[^']*')|[\[\]{}]""", body):
        value = token[0]
        if token[1] and depth and not objects:
            if match := re.match(r"([A-Za-z0-9][A-Za-z0-9._-]*)(?:\[|\s*[<>=!~;@]|$)", value[1:-1]):
                names.add(match[1])
        elif value in {"[", "]"}:
            depth += 1 if value == "[" else -1
        elif value in {"{", "}"}:
            objects += 1 if value == "{" else -1
    return names


def recipe_error(tool: str, args: list[str], directory: Path, root: Path) -> str | None:
    # Flags selecting another file/directory change the namespace; do not guess their value.
    if any(arg.startswith(("-C", "-f", "--directory", "--file", "--makefile", "--justfile")) for arg in args):
        return None
    targets = [
        arg
        for index, arg in enumerate(args)
        if not arg.startswith("-")
        and "=" not in arg
        and not (
            tool == "make"
            and index > 0
            and args[index - 1] in {"-j", "--jobs", "-l", "--load-average", "--max-load"}
            and re.fullmatch(r"\d+(?:\.\d+)?", arg)
        )
    ]
    if tool == "just":
        targets = targets[:1]  # Later arguments can be recipe parameters.
    names = (
        ("GNUmakefile", "makefile", "Makefile") if tool == "make" else ("justfile", "Justfile", ".justfile")
    )
    manifest = nearest(directory, root, names)
    body = manifest.read_text(encoding="utf-8") if manifest else ""
    pattern = r"(?m)^([^\s#:=][^:=\n]*):(?!=)" if tool == "make" else r"(?m)^([\w-]+)[^:\n]*:(?!=)"
    definitions = {name for match in re.finditer(pattern, body) for name in match[1].split()}
    for name in targets:
        if any(char in name for char in "$*`%"):
            continue
        if name not in definitions and not (tool == "make" and (directory / name).exists()):
            kind = "target" if tool == "make" else "recipe"
            return f"{tool} {name}: no {kind} in nearest {manifest.name if manifest else names[-1]}"
    return None


def task_error(words: list[str], directory: Path, root: Path) -> str | None:
    tool, *args = words
    if tool in {"npm", "pnpm", "yarn", "bun"} and len(args) >= 2 and args[0] == "run":
        name = args[1]
        if name.startswith("-") or any(char in name for char in "$*`"):
            return None
        if tool == "bun" and (directory / name).is_file():
            return None  # Bun runs a file directly when no script has that name.
        manifest = nearest(directory, root, ("package.json",))
        data = json.loads(manifest.read_text(encoding="utf-8")) if manifest else {}
        scripts = data.get("scripts", {}) if isinstance(data, dict) else {}
        if not isinstance(scripts, dict) or name not in scripts:
            return f"{tool} run {name}: no script in nearest package.json"
    if tool in {"make", "just"}:
        return recipe_error(tool, args, directory, root)
    if tool == "uv" and len(args) >= 2 and args[0] == "run" and not args[1].startswith("-"):
        name = args[1]
        if not path_like(name, command=True) and not any(char in name for char in "$`<>"):
            manifest = nearest(directory, root, ("pyproject.toml",))
            normalized = re.sub(r"[-_.]+", "-", name).lower()
            scripts, dependencies = python_tools(manifest) if manifest else (set(), set())
            if (
                not available_tool(name, directory, root)
                and name not in scripts
                and normalized not in dependencies
            ):
                return f"uv run {name}: no project script, dependency, or executable on PATH"
    return None


def command_paths(words: list[str]) -> list[str]:
    if words[:2] == ["uv", "run"] and len(words) > 2 and not words[2].startswith("-"):
        words = words[2:]
    result = [words[0]] if path_like(words[0], command=True) else []
    if re.fullmatch(r"python(?:\d+(?:\.\d+)*)?|luajit|lua|node|ruby|sh|bash|zsh", words[0]):
        args = words[1:]
        if any(arg in {"-m", "-c", "-e", "--eval", "--command"} for arg in args):
            return result
        script = next((arg for arg in args if not arg.startswith("-")), "")
        if path_like(script, command=True):
            result.append(script)
    return result


def shell_lines(lines: list[tuple[int, str]]) -> Iterator[tuple[int, list[str]]]:
    pending = ""
    start = 1
    for number, line in lines:
        if not pending:
            start = number
        if line.rstrip().endswith("\\"):
            pending += line.rstrip()[:-1] + " "
            continue
        for words in command_segments(pending + line):
            yield start, words
        pending = ""


def tree_listing(lines: list[tuple[int, str]]) -> bool:
    if any(re.match(r"\s*(?:[│| ]*[├└][─━]|[| ]*[+`\\]--)", line) for _, line in lines):
        return True
    entries = [line.split("#", 1)[0].rstrip() for _, line in lines if line.split("#", 1)[0].strip()]
    return bool(
        len(entries) > 1
        and all(re.fullmatch(r"\s*[\w./-]+", line) for line in entries)
        and any(
            parent.strip().endswith("/")
            and len(child) - len(child.lstrip()) > len(parent) - len(parent.lstrip())
            for parent, child in pairwise(entries)
        )
    )


def command_issues(root: Path, file: str, document: Markdown) -> list[Issue]:
    result = []
    for _, tag, lines in document.blocks:
        if tag not in SHELL_TAGS or tree_listing(lines):
            continue
        directory: Path | None = (root / file).parent
        for start, words in shell_lines(lines):
            if directory is None:
                break
            tool = words[0]
            if error := task_error(words, directory, root):
                result.append(issue("dead-script", file, start, error))
            for value in (path for arg in command_paths(words) for path in expand_braces(arg)):
                # A command runs from the block's working directory, never from root as a fallback.
                if existing_path(value, directory, directory) is None:
                    result.append(issue("dead-script", file, start, f"command path does not exist: {value}"))
            if (
                tool not in BUILTINS
                and "/" not in tool
                and not path_like(tool, command=True)
                and not any(c in tool for c in "$`<>")
                and not available_tool(tool, directory, root)
            ):
                result.append(issue("tool-not-found", file, start, f"not on PATH: {tool}", "note"))
            if tool == "cd":
                # A static cd persists for this block; unknown working directories are skipped.
                directory = (
                    (directory / words[1]).resolve()
                    if len(words) == 2 and (directory / words[1]).is_dir()
                    else None
                )
    return result


def pack_requirements(skill: Path) -> list[dict[str, str]]:
    result = []
    parsed = markdown(skill.read_text(encoding="utf-8"))
    for begin, heading, finish in parsed.sections():
        if heading.casefold() != "requirements":
            continue
        for text in parsed.prose[begin + 1 : finish]:
            if match := re.match(r"\s*[-+*]\s+\*\*([A-Za-z0-9][A-Za-z0-9_.-]*)\*\*\s*(.*)", text):
                result.append({"id": match[1], "text": match[2].strip()})
    return result


def declared_name(line: str) -> str:
    tokens = sorted([*LINK.finditer(line), *INLINE_CODE.finditer(line)], key=lambda match: match.start())
    if not tokens:
        return ""
    token = tokens[0]
    return token[1].strip("` ") if token.re == LINK else token[2].strip()


class StandardError(Exception):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


def fetch_standard(url: str, sha: str, path: str, destination: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="sift-standards-") as directory:

        def run(*args: str) -> bytes:
            return subprocess.run(
                ["git", *args],
                cwd=directory,
                capture_output=True,
                check=True,
                timeout=60,
                env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
            ).stdout

        run("init", "-q")
        run("fetch", "-q", "--depth", "1", "--filter=blob:none", url, sha)
        entries = run("ls-tree", "-r", "-l", "-z", f"{sha}:{path}").split(b"\0")[:-1]
        if len(entries) > MAX_STANDARD_FILES:
            raise ValueError(f"pack exceeds {MAX_STANDARD_FILES} files")
        files = []
        total = 0
        for entry in entries:
            metadata, filename = entry.split(b"\t", 1)
            mode, kind, oid, size = metadata.split()
            name = os.fsdecode(filename)
            relative = Path(name)
            if (
                relative.is_absolute()
                or relative.drive
                or "\\" in name
                or any(part in {"", ".", "..", ".git"} for part in name.split("/"))
            ):
                raise ValueError(f"unsafe pack path: {name!r}")
            if mode not in {b"100644", b"100755"} or kind != b"blob":
                raise ValueError(f"pack entry must be a regular file (no symlinks or submodules): {name!r}")
            total += int(size)
            if total > MAX_STANDARD_BYTES:
                raise ValueError(f"pack exceeds {MAX_STANDARD_BYTES} bytes")
            files.append((relative, mode, oid.decode("ascii")))
        if not any(relative == Path("SKILL.md") for relative, _, _ in files):
            raise ValueError("pack is missing SKILL.md")
        for relative, mode, oid in files:
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(run("cat-file", "blob", oid))
            target.chmod(0o755 if mode == b"100755" else 0o644)


def pinned_standard(name: str, url: str, overrides: list[Path], *, offline: bool) -> tuple[Path, str]:
    url_parts = urlsplit(url).path.strip("/").split("/")
    parts = [unquote(part) for part in url_parts]
    if (
        len(parts) < 5
        or parts[2] != "tree"
        or any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", part) for part in parts[:2])
        or any(part in {"", ".", ".."} or "/" in part or "\\" in part or "\0" in part for part in parts[4:])
    ):
        raise StandardError("bad-standard", f"{name}: invalid GitHub tree URL: {url}")
    owner, repo, _, ref, *segments = parts
    if segments[-1] != name:
        raise StandardError(
            "bad-standard", f"{name}: URL's final path segment must equal the pack name: {url}"
        )
    path = "/".join(segments)
    if not re.fullmatch(r"[0-9a-fA-F]{40}", ref):
        repository = f"https://github.com/{owner}/{repo}"
        command = shlex.join(["git", "ls-remote", repository, f"refs/heads/{ref}"])
        raise StandardError(
            "unpinned-standard",
            f"{name}: pin a full 40-hex commit SHA; run `{command}` and rewrite the URL as "
            f"{repository}/tree/<40-hex-commit-sha>/{'/'.join(url_parts[4:])}",
        )
    override = next((base / name for base in overrides if (base / name / "SKILL.md").is_file()), None)
    if override is not None:
        return override, "override"
    cache_root = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache").expanduser()
    # v2 caches whole directories; legacy SKILL.md-only entries are not complete packs.
    skill = cache_root / "sift/standards-v2" / owner / repo / ref.lower() / path / "SKILL.md"
    if skill.is_file():
        return skill.parent, "cache"
    if offline:
        raise StandardError(
            "unknown-standard",
            f"{name}: pinned pack unavailable offline: {url}; run without --offline to fetch it "
            "or set SIFT_STANDARDS_PATH",
        )
    git_url = os.environ.get("SIFT_STANDARDS_GIT_URL", "https://github.com/{owner}/{repo}.git").format(
        owner=owner, repo=repo
    )
    try:
        skill.parent.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".sift-", dir=skill.parent.parent) as directory:
            pending = Path(directory) / name
            pending.mkdir()
            fetch_standard(git_url, ref.lower(), path, pending)
            try:
                os.replace(pending, skill.parent)
            except OSError:
                if not skill.is_file():
                    raise
                # A concurrent fetch has already published the complete directory.
                return skill.parent, "cache"
    except (OSError, ValueError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        stderr = os.fsdecode(getattr(error, "stderr", None) or b"").splitlines()
        detail = stderr[0] if stderr else str(error)
        raise StandardError(
            "standard-fetch-failed", f"{name}: failed to fetch {url} ({git_url}): {detail}"
        ) from error
    return skill.parent, "fetched"


def standard_items(document: Markdown) -> Iterator[tuple[int, str]]:
    for start, title, end in document.sections():
        if title.casefold() != "standards":
            continue
        number, item = 0, []
        for index in range(start + 1, end):
            line = document.prose[index]
            if re.match(r" {0,3}(?:[-+*]|\d+[.)])\s+", line):
                if item:
                    yield number, "\n".join(item)
                number, item = index + 1, [line]
            elif item and (not line.strip() or line.startswith((" ", "\t"))):
                item.append(line)
            elif item:
                yield number, "\n".join(item)
                item = []
        if item:
            yield number, "\n".join(item)


def standards(
    root: Path, document: Markdown, *, offline: bool = False
) -> tuple[list[dict[str, Any]], list[Issue]]:
    result, issues = [], []
    overrides = [
        Path(part).expanduser()
        for part in os.environ.get("SIFT_STANDARDS_PATH", "").split(os.pathsep)
        if part
    ]
    search = [
        root / ".agents/skills",
        root / ".claude/skills",
        *overrides,
        Path.home() / ".agents/skills",
        Path.home() / ".claude/skills",
    ]
    for number, item in standard_items(document):
        name = declared_name(item)
        if not PACK_NAME.fullmatch(name):
            issues.append(
                issue(
                    "unknown-standard",
                    "AGENTS.md",
                    number,
                    f"malformed standards declaration: {item.strip()}",
                )
            )
            continue
        record: dict[str, Any] = {"name": name, "path": None, "source": None, "requirements": []}
        result.append(record)
        try:
            if match := GITHUB_URL.search(item):
                pack, source = pinned_standard(name, match[0].rstrip(".,;:"), overrides, offline=offline)
            else:
                pack = next((base / name for base in search if (base / name).exists()), None)
                if pack is None:
                    raise StandardError(
                        "unknown-standard",
                        f"standards pack not installed: {name}; "
                        "pin a GitHub tree URL with a full 40-hex commit SHA",
                    )
                source = "local"
            record.update(path=str(pack.resolve()), source=source)
            if source == "override":
                issues.append(
                    issue(
                        "standard-local-override",
                        "AGENTS.md",
                        number,
                        f"{name}: using local override from SIFT_STANDARDS_PATH: {pack.resolve()}",
                        "note",
                    )
                )
        except StandardError as error:
            issues.append(issue(error.code, "AGENTS.md", number, str(error)))
            continue
        skill = pack / "SKILL.md"
        error = frontmatter_error(skill)
        if error is None:
            record["requirements"] = pack_requirements(skill)
            if not record["requirements"]:
                error = "missing requirement ids under ## Requirements"
        if error:
            issues.append(issue("bad-standard", "AGENTS.md", number, f"{name}: {error}"))
    return result, issues


def check_file(root: Path, file: str, content: str, max_lines: int) -> list[Issue]:
    document = markdown(content)
    result = []
    if not has_summary(document):
        result.append(
            issue("missing-summary", file, 1, "add introductory prose or an opening overview/summary H2")
        )
    sections = list(document.sections())
    if not any(
        re.search(r"command|check|gate|build|test|develop", title, re.I)
        and any(start < number - 1 < end for number, _, _ in document.blocks)
        for start, title, end in sections
    ):
        result.append(
            issue("missing-commands", file, 1, "add a commands/checks H2 section with a fenced code block")
        )
    elif (
        file == "AGENTS.md"
        and (root / ".sift" / "gate.py").is_file()
        and not any(".sift/gate.py" in text for _, _, lines in document.blocks for _, text in lines)
    ):
        result.append(issue("missing-gate", file, 1, "add the .sift/gate.py command to the checks block"))
    if not any(
        re.search(r"rule|convention|boundar|constraint|gotcha", title, re.I) for _, title, _ in sections
    ):
        result.append(issue("missing-rules", file, 1, "add a rules/conventions H2 section"))
    if len(content.splitlines()) > max_lines:
        result.append(
            issue(
                "over-budget",
                file,
                max_lines + 1,
                f"exceeds {max_lines}-line budget; move detail to linked files",
            )
        )
    result.extend(reference_issues(root, file, document))
    result.extend(command_issues(root, file, document))
    sibling = (root / file).with_name("CLAUDE.md")
    if sibling.exists() or sibling.is_symlink():
        same = sibling.is_symlink() and sibling.resolve() == (root / file).resolve()
        imported = sibling.is_file() and any(
            line.strip() == "@AGENTS.md" for line in sibling.read_text(encoding="utf-8").splitlines()
        )
        if not same and not imported:
            result.append(
                issue(
                    "claude-md",
                    file,
                    1,
                    "CLAUDE.md must symlink to AGENTS.md or contain an @AGENTS.md import line",
                )
            )
    return result


def byte_budget_issues(contents: dict[str, str]) -> list[Issue]:
    # Codex consumes raw file bytes root -> cwd, skipping whitespace-only files. Assembly
    # separators do not consume the budget (verified 2026-09-24):
    # https://github.com/openai/codex/blob/29f056c26c09b51db123069ed3ec2095b227d6db/codex-rs/core/src/agents_md.rs
    # Default: codex-rs/config/src/config_toml.rs, DEFAULT_PROJECT_DOC_MAX_BYTES = 32 * 1024.
    result = []
    for file in contents:
        chain = [
            (name, contents[name])
            for directory in reversed(Path(file).parents)
            if (name := (directory / "AGENTS.md").as_posix()) in contents and contents[name].strip()
        ]
        size = sum(len(content.encode("utf-8")) for _, content in chain)
        if size <= NOTE_BYTES:
            continue
        message = f"root-to-directory AGENTS.md chain is {size} bytes"
        if size <= MAX_BYTES:
            result.append(
                issue("bytes-note", file, 1, f"{message}; above {NOTE_BYTES}-byte guidance", "note")
            )
            continue
        offset = 0
        affected = past_cut = None
        cut_line = 1
        for name, content in chain:
            offsets = [offset]
            for line in content.splitlines(keepends=True):
                offsets.append(offsets[-1] + len(line.encode("utf-8")))
            if name == file and offset <= MAX_BYTES < offsets[-1]:
                cut_line = next(i for i, end in enumerate(offsets[1:], 1) if end > MAX_BYTES)
            for start, title, end in markdown(content).sections():
                section = f"{title!r} ({name}:{start + 1})"
                if affected is None and offsets[end] > MAX_BYTES:
                    affected = section
                if past_cut is None and offsets[start] >= MAX_BYTES:
                    past_cut = section
            offset = offsets[-1]
        message += f"; exceeds {MAX_BYTES}-byte budget"
        message += f"; first H2 affected: {affected}" if affected else "; no H2 section past the cut"
        if past_cut and past_cut != affected:
            message += f"; first H2 starting past the cut: {past_cut}"
        result.append(issue("over-bytes", file, cut_line, message))
    return result


def issue_key(item: Issue) -> str:
    return f"{item['code']}|{item['file']}|{item['message']}"


def read_baseline(path: Path) -> set[str]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if (
        not isinstance(data, dict)
        or type(data.get("schema_version")) is not int
        or data["schema_version"] != 1
        or not isinstance(data.get("issues"), list)
        or any(not isinstance(key, str) or len(key.split("|", 2)) != 3 for key in data["issues"])
    ):
        raise ValueError("baseline must have schema_version 1 and an issues array of code|file|message keys")
    return set(data["issues"])


def apply_baseline(issues: list[Issue], keys: set[str]) -> int:
    current = {issue_key(item) for item in issues if item["severity"] == "error"}
    count = 0
    for item in issues:
        item["baselined"] = item["severity"] == "error" and issue_key(item) in keys
        count += item["baselined"]
    for key in sorted(keys - current):
        code, file, message = key.split("|", 2)
        issues.append(
            {
                **issue("baseline-fixed", file, 1, f"fixed; remove from baseline: {code}: {message}", "note"),
                "baselined": False,
            }
        )
    return count


def check(root: Path, max_lines: int = 300, *, offline: bool = False) -> dict[str, Any]:
    files = enumerate_files(root)
    issues = []
    packs = []
    contents = {}
    if not (root / "AGENTS.md").is_file():
        issues.append(issue("missing-root", "AGENTS.md", 1, "root AGENTS.md is missing"))
    for file in files:
        # Git also lists tracked deletions, which have no instructions left to inspect.
        if not (root / file).exists() and not (root / file).is_symlink():
            continue
        content = (root / file).read_bytes().decode("utf-8")
        contents[file] = content
        issues.extend(check_file(root, file, content, max_lines))
        if file == "AGENTS.md":
            packs, errors = standards(root, markdown(content), offline=offline)
            issues.extend(errors)
    issues.extend(byte_budget_issues(contents))
    return {"files": files, "issues": issues, "baselined": 0, "standards": packs}


def print_human(result: dict[str, Any], *, show_standards: bool) -> None:
    if show_standards:
        for pack in result["standards"]:
            source = f" ({pack['source']})" if pack["source"] else ""
            print(f"{pack['name']}: {pack['path'] or 'not found'}{source}")
            for requirement in pack["requirements"]:
                print(f"  {requirement['id']}: {requirement['text']}")
    for item in result["issues"]:
        status = " [baselined]" if item["baselined"] else " [note]" if item["severity"] == "note" else ""
        print(f"{item['file']}:{item['line']}: {item['code']}: {item['message']}{status}")
    errors = sum(item["severity"] == "error" and not item["baselined"] for item in result["issues"])
    notes = sum(item["severity"] == "note" for item in result["issues"])
    print(
        f"Totals: {len(result['files'])} files, {errors} new errors, "
        f"{notes} notes, {result['baselined']} baselined"
    )


def main() -> int:
    plain: dict[str, Any] = {"color": False} if sys.version_info >= (3, 14) else {}
    parser = argparse.ArgumentParser(description=__doc__, **plain)
    subparsers = parser.add_subparsers(dest="command", required=True)
    checking = subparsers.add_parser("check", help="check AGENTS.md files", **plain)
    listing = subparsers.add_parser("standards", help="resolve declared standards packs", **plain)
    for subparser in (checking, listing):
        subparser.add_argument("--root", type=Path, help="repository root (default: git toplevel or cwd)")
        subparser.add_argument("--json", action="store_true", dest="as_json")
        subparser.add_argument("--offline", action="store_true", help="resolve standards without fetching")
    checking.add_argument("--baseline", type=Path, help="baseline path (relative to root)")
    checking.add_argument("--write-baseline", action="store_true")
    checking.add_argument("--max-lines", type=int, default=300)
    result: dict[str, Any]
    args = parser.parse_args()
    root = (args.root or default_root()).resolve()
    if not root.is_dir():
        parser.error("--root must be an existing directory")
    if args.command == "check" and args.max_lines < 1:
        parser.error("--max-lines must be positive")
    try:
        if args.command == "check":
            result = check(root, args.max_lines, offline=args.offline)
            baseline = root / (args.baseline or ".sift/agents-baseline.json")
            keys = (
                read_baseline(baseline)
                if baseline.exists() or (args.baseline and not args.write_baseline)
                else set()
            )
            if args.write_baseline:
                keys = {issue_key(item) for item in result["issues"] if item["severity"] == "error"}
                baseline.parent.mkdir(parents=True, exist_ok=True)
                baseline.write_text(
                    json.dumps({"schema_version": 1, "issues": sorted(keys)}, indent=2) + "\n",
                    encoding="utf-8",
                )
            result["baselined"] = apply_baseline(result["issues"], keys)
        else:
            path = root / "AGENTS.md"
            packs, issues = (
                standards(root, markdown(path.read_text(encoding="utf-8")), offline=args.offline)
                if path.is_file()
                else ([], [issue("missing-root", "AGENTS.md", 1, "root AGENTS.md is missing")])
            )
            result = {
                "files": ["AGENTS.md"] if path.is_file() else [],
                "issues": issues,
                "baselined": 0,
                "standards": packs,
            }
            apply_baseline(issues, set())
    except (OSError, UnicodeError, ValueError) as error:
        parser.error(str(error))
    result["issues"].sort(key=lambda item: (item["file"], item["line"], item["code"], item["message"]))
    if args.as_json:
        print(json.dumps(result, indent=2, ensure_ascii=True))
    else:
        print_human(result, show_standards=args.command == "standards")
    return int(any(item["severity"] == "error" and not item["baselined"] for item in result["issues"]))


if __name__ == "__main__":
    raise SystemExit(main())
