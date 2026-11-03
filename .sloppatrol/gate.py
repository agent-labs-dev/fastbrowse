#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Agent Labs
# SPDX-License-Identifier: MIT
"""Run a repository's rules and tests; filter their hits through shared markers."""

from __future__ import annotations

import argparse
import concurrent.futures
import fnmatch
import functools
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path
from typing import Any

sys.dont_write_bytecode = True
VERSION = "0.1.0"
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
CONFIG = ".sloppatrol/sgconfig.yml"
SCRIPT_TIMEOUT = 600.0
BUILTIN_LENSES: frozenset[str] = frozenset(
    "comment-narration copy-slop dead-code defensive-noise oversized-modules parallel-implementations "  # noqa: SIM905 (a word list reads best as one string)
    "reader-journey reinvented-wheel session-residue shallow-modules silent-fallbacks "
    "hidden-dependency interface-leak speculative-abstraction stale-docs "
    "stringly-typed test-plumbing wall-of-text".split()
)
COMMENT_EXTENSIONS = {
    "#": "py pyi py3 bzl sh bash zsh ksh bats rb rbw gemspec yaml yml toml r pl ex exs nix tf hcl dockerfile "
    "makefile toc",
    "//": "js jsx ts tsx mjs cjs mts cts go rs c h cpp hpp cc cxx hxx hh c++ cu ino java kt kts ktm swift cs "
    "scala sc sbt dart zig php sol",
    "--": "lua sql hs elm",
    ";": "clj el lisp asm ini",
    "%": "tex erl m",
    "<!--": "html htm xhtml xml vue svelte",
}
COMMENT_BY_EXTENSION = {ext: token for token, exts in COMMENT_EXTENSIONS.items() for ext in exts.split()}
NAMED_HASH = frozenset(
    "makefile gnumakefile .pkgmeta .gitignore .gitignore_global .npmignore .dockerignore .ignore .rgignore "  # noqa: SIM905 (a word list reads best as one string)
    ".fdignore .prettierignore .eslintignore .stylelintignore .gitattributes".split()
)
GENERIC_REASONS = frozenset(
    "false positive,intentional,legacy,needed,ok,safe,todo,temporary,see above,as above".split(",")  # noqa: SIM905 (a word list reads best as one string)
)
PR_EVENTS = frozenset({"pull_request", "pull_request_target", "merge_group"})
ID = r"[a-z0-9][a-z0-9-]*"
SCRIPT_TEST_CASE = re.compile(r"(?:^|/)script-tests/[^/]+/[^/]+/")
QUOTED = r""""(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`"""
RETIRED_DIRS = (".sloppatrol/rules/", ".sloppatrol/scripts/")
RETIRED_EXCLUDED = (
    ".sloppatrol/reports/",
    ".sloppatrol/runs/",
    ".sloppatrol/ledger.json",
    ".sloppatrol/rule-tests/",
    ".sloppatrol/script-tests/",
)
MIN_RETIRED_ID = 8


def comment_token(path: str) -> str | None:
    name = Path(path).name.lower()
    if name == ".luacheckrc":
        return "--"
    if name in NAMED_HASH or name.startswith(("dockerfile", "makefile.", "gnumakefile.")):
        return "#"
    return COMMENT_BY_EXTENSION.get(Path(path).suffix.lstrip(".").lower())


def comment(path: str, line: str) -> tuple[str, bool] | None:
    token = comment_token(path)
    if not token:
        return None
    pattern = re.escape(token) + (r"|/\*" if token == "//" else "")
    for match in re.finditer(rf"{QUOTED}|(?P<comment>{pattern})(?P<body>.*)", line):
        if match["comment"]:
            body = match["body"].strip()
            whole = not line[: match.start()].strip() and match["comment"] == token
            if token == "<!--" or match["comment"] == "/*":
                body, closing, after = body.partition("-->" if token == "<!--" else "*/")
                whole = whole and bool(closing) and not after.strip()
            return body.strip(), whole
    return None


MARKER_START = r"(?<![\w./-])sloppatrol:"


def scan_markers(path: str, text: str, ids: set[str] | frozenset[str]) -> list[dict[str, Any]]:
    """Return {line, id, reason, error}; comment-shaped multiline-string lines count as comments."""
    markers: list[dict[str, Any]] = []
    if "sloppatrol:" not in text and "ast-grep-ignore" not in text:  # every marker and marker error names one
        return markers
    for number, line in enumerate(text.split("\n"), 1):
        parsed = comment(path, line)
        if parsed is None:
            continue
        body, whole = parsed
        if "ast-grep-ignore" in body:
            markers.append(dict(line=number, id=None, reason="", error="sloppatrol-ast-grep-ignore"))
        # The marker word inside a path or another word, such as a .sloppatrol directory, is prose.
        if not (named := re.search(rf"{MARKER_START}\s*({ID})?", body)):
            continue
        match = re.fullmatch(rf"sloppatrol: ({ID}) -(?: (.*))?", body)
        rule_id, reason = (match[1], match[2] or "") if match else (named[1], "")
        words = re.findall(r"[^\W_]+", reason.casefold())
        error = None
        if not whole:
            error = "sloppatrol-trailing-marker"
        elif not match:
            error = "sloppatrol-bad-marker"
        elif rule_id not in ids or rule_id.startswith("sloppatrol-"):
            error = "sloppatrol-unknown-id"
        elif len(words) < 3 or " ".join(words) in GENERIC_REASONS:
            error = "sloppatrol-weak-reason"
        markers.append(dict(line=number, id=rule_id, reason=reason, error=error))
    return markers


def marker_covers(path: str, text: str, marker_line: int, hit_line: int) -> bool:
    """Coverage only: callers first check the marker's validity and id against the hit.

    Lines split on newline alone, as ast-grep counts them. A line-1 hit is a whole-file fact, so the
    file's leading comment block (after any #! line) covers it too.
    """
    if hit_line - 3 <= marker_line <= hit_line:
        return True
    lines = text.split("\n")
    start, end = (
        (int(lines[0].startswith("#!")), marker_line) if hit_line == 1 else (marker_line - 1, hit_line - 1)
    )
    return 0 <= start < end <= len(lines) and all(
        (parsed := comment(path, line)) is not None and parsed[1] for line in lines[start:end]
    )


def glob_matches(pattern: str, path: str) -> bool:
    return glob_regex(pattern).fullmatch(path) is not None


@functools.cache
def glob_regex(pattern: str) -> re.Pattern[str]:
    """globset's separator-crossing wildcards, component globstars, classes and alternatives."""
    pattern = re.sub(r"\{([^{}]*)\}", lambda m: "{" + ",".join(filter(None, m[1].split(","))) + "}", pattern)
    parts, depth = [], 0
    for match in re.finditer(r"\\.|\*\*/|\*+|\?|\[!?]?[^\]]*\]|[{},]|.", pattern):
        token = match[0]
        if token == "**/" and (match.start() == 0 or pattern[match.start() - 1] == "/"):
            parts.append("(?:.*/)?")
        elif token.startswith("*"):
            parts.append(".*" + ("/" if token.endswith("/") else ""))
        elif token == "?":
            parts.append(".")
        elif token.startswith("["):
            parts.append(fnmatch.translate(token.replace("[^", "[!", 1))[4:-3])
        elif token == "{":
            parts.append("(?:")
            depth += 1
        elif token == "}" and depth:
            parts.append(")")
            depth -= 1
        elif token == "," and depth:
            parts.append("|")
        else:
            parts.append(re.escape(token.removeprefix("\\")))
    try:
        return re.compile("".join(parts))
    except re.error as error:
        raise GateError(f"invalid files glob {pattern}: {error}") from error


class GateError(Exception):
    def __init__(self, message: str, code: str = "sloppatrol-config"):
        super().__init__(message)
        self.code = code


def run(args: list[str], root: Path, **kwargs: Any) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=root, capture_output=True, text=True, check=False, **kwargs)


def git(root: Path, *args: str, env: dict[str, str] | None = None) -> str:
    result = run(["git", *args], root, env=env)
    if result.returncode:
        raise GateError(result.stderr.strip())
    return result.stdout.strip("\n")


def retired_ids(root: Path) -> dict[str, str]:
    """Return rule and script ids deleted from history and absent from the current tree."""
    result = run(
        [
            "git",
            "log",
            "--no-merges",
            "--reverse",
            "--diff-filter=D",
            "--format=%H",
            "--name-only",
            "--",
            *RETIRED_DIRS,
        ],
        root,
    )
    if result.returncode:
        if "does not have any commits yet" in result.stderr:
            return {}
        raise GateError(result.stderr.strip())
    output = result.stdout
    deleted: dict[str, str] = {}
    sha = ""
    for line in output.splitlines():
        if re.fullmatch(r"[0-9a-f]{40}", line):
            sha = line
            continue
        if not sha or not any(line.startswith(directory) for directory in RETIRED_DIRS):
            continue
        path = Path(line)
        if path.parent.as_posix() not in {directory.rstrip("/") for directory in RETIRED_DIRS}:
            continue
        deleted[path.stem] = sha
    live = {
        path.stem for directory in RETIRED_DIRS for path in (root / directory).glob("*") if path.is_file()
    }
    return {
        rule_id: sha
        for rule_id, sha in deleted.items()
        if rule_id not in live and len(rule_id) >= MIN_RETIRED_ID and re.search(r"[-_]", rule_id)
    }


def retired_findings(root: Path) -> list[dict[str, Any]]:
    """Find current tracked files that still name a deleted rule or script id."""
    findings = []
    for rule_id, sha in sorted(retired_ids(root).items()):
        result = run(["git", "grep", "-n", "-F", "-e", rule_id, "HEAD", "--"], root)
        if result.returncode not in (0, 1):
            raise GateError(result.stderr.strip())
        for match in result.stdout.splitlines():
            if not match.startswith("HEAD:"):
                continue
            name_line = match[5:]
            try:
                path, line, _ = name_line.rsplit(":", 2)
                number = int(line)
            except ValueError:
                path, number = name_line, 1
            if any(path.startswith(prefix) or path == prefix.rstrip("/") for prefix in RETIRED_EXCLUDED):
                continue
            if "CHANGELOG" in Path(path).name.upper():
                continue
            findings.append(
                finding(
                    "sloppatrol-retired-id",
                    "gate",
                    path,
                    number,
                    f"{rule_id} was deleted in {sha}",
                )
            )
            break
    return findings


def scalar(value: str) -> str:
    value = value.strip()
    if value.startswith('"'):
        decoder = json.JSONDecoder()
        parsed, end = decoder.raw_decode(value)
        if value[end:].strip() and not value[end:].lstrip().startswith("#"):
            raise ValueError("text after quoted scalar")
        return parsed
    if value.startswith("'"):
        match = re.fullmatch(r"'((?:[^']|'')*)'\s*(?:#.*)?", value)
        if not match:
            raise ValueError("invalid quoted scalar")
        return match[1].replace("''", "'")
    value = re.split(r"\s+#", value, maxsplit=1)[0]
    if not value or value[0] in "[{&*!|>" or ": " in value:
        raise ValueError("expected a plain or quoted scalar")
    return value


def yaml_fields(path: Path, keys: set[str]) -> dict[str, Any]:  # noqa: PLR0912 -- bounded YAML surface
    """Read the gate's small YAML surface; ast-grep owns every other key."""
    lines, fields = path.read_text().splitlines(), {}
    starts = [i for i, line in enumerate(lines) if line and not line[0].isspace() and line[0] not in "#-"]
    for index, end in zip(starts, [*starts[1:], len(lines)], strict=True):
        match = re.fullmatch(r"([\w-]+):\s*(.*)", lines[index])
        if not match or match[1] not in keys:
            continue
        key, value = match.groups()
        body = [line for line in lines[index + 1 : end] if line.strip() and not line.lstrip().startswith("#")]
        try:
            if key in fields:
                raise ValueError(f"duplicate {key}")
            if key == "metadata":
                if value and not value.startswith("#"):
                    raise ValueError("metadata must be a block mapping")
                scope = [line.strip() for line in body if re.match(r"\s+sloppatrol-scope\s*:", line)]
                if len(scope) > 1:
                    raise ValueError("duplicate sloppatrol-scope")
                fields[key] = scalar(scope[0].split(":", 1)[1]) if scope else "all"
            elif key in {"valid", "invalid"}:  # ast-grep parses the cases; the gate needs one of each
                flow = " ".join([value, *(line.strip() for line in body)])
                fields[key] = bool(re.match(r"\[\s*[^\s\]]", flow)) or any(
                    line.lstrip().startswith("- ") for line in body
                )
            elif key == "files":
                if value.startswith("["):
                    match_list = re.fullmatch(r"\[(.*)\]\s*(?:#.*)?", value)
                    if not match_list or body:
                        raise ValueError("expected a one-line flow list")
                    items = re.findall(r"""(?:"(?:\\.|[^"\\])*"|'(?:[^']|'')*'|[^,])+""", match_list[1])
                else:
                    items = [line.strip()[2:] for line in body if line.lstrip().startswith("- ")]
                    if (value and not value.startswith("#")) or len(items) != len(body):
                        raise ValueError("expected a block or flow list of scalars")
                fields[key] = [scalar(item) for item in items]
            elif re.fullmatch(r"[|>][-+]?", value):
                fields[key] = ("\n" if value.startswith("|") else " ").join(line.strip() for line in body)
            else:
                if body:
                    raise ValueError("unexpected continuation")
                fields[key] = scalar(value)
        except (ValueError, TypeError) as error:
            raise GateError(f"{path}:{index + 1}: {error}") from error
    return fields


def ls_z(root: Path, *args: str, env: dict[str, str] | None = None) -> list[str]:
    paths = [p for p in git(root, args[0], "-z", *args[1:], env=env).split("\0") if p]
    if any("\t" in p or "\n" in p for p in paths):
        raise GateError("path contains a TAB or newline")
    return paths


def corpus(root: Path, env: dict[str, str] | None = None) -> list[str]:
    """Script-test cases are inputs wherever they live in the repository.

    They are excluded from the corpus after discovery so fixtures cannot report their own findings.
    """
    paths = ls_z(root, "ls-files", "-co", "--exclude-standard", env=env)
    return sorted({p for p in paths if not SCRIPT_TEST_CASE.search(p) and (root / p).is_file()})


def discover(root: Path, paths: list[str]) -> tuple[dict[str, Any], set[str]]:  # noqa: PLR0912 (one branch per layout check)
    rules, ids = {}, set(BUILTIN_LENSES)
    sloppatrol = root / ".sloppatrol"
    for path in sorted(
        [
            *(sloppatrol / "rules").glob("*.yml"),
            *(sloppatrol / "scripts").glob("*"),
            *(sloppatrol / "lenses").glob("*.md"),
        ]
    ):
        rule_id = path.stem
        if not re.fullmatch(ID, rule_id) or rule_id.startswith("sloppatrol-") or rule_id in ids:
            raise GateError(f"{path}: invalid, reserved or duplicate id {rule_id}")
        ids.add(rule_id)
        if path.parent.name == "lenses":
            continue
        if path.parent.name == "rules":
            data = yaml_fields(path, {"id", "severity", "message", "note", "files", "metadata"})
            if data.get("id") != rule_id or data.get("severity", "error") != "error":
                raise GateError(f"{path}: id must equal stem; severity must be absent or error")
            if not data.get("message") or not data.get("note"):
                raise GateError(f"{path}: message and note are required")
            test = sloppatrol / "rule-tests" / f"{rule_id}-test.yml"
            fields = yaml_fields(test, {"id", "valid", "invalid"}) if test.is_file() else {}
            if fields.get("id") != rule_id or not fields.get("valid") or not fields.get("invalid"):
                raise GateError(f"{test}: matching id and nonempty valid and invalid lists required")
            if "files" in data and not any(glob_matches(g, p) for g in data["files"] for p in paths):
                raise GateError(f"{path}: files globs match no corpus path")
            data.update(kind="ast-grep", scope=data.get("metadata", "all"))
        else:
            if not path.is_file() or path.is_symlink() or not os.access(path, os.X_OK):
                raise GateError(f"{path}: script must be an executable regular file")
            lines, headers = path.read_text().splitlines(), {}
            for line in lines[:40]:
                if match := re.match(
                    r"\s*(?:#|//|--|;|%|<!--|/\*)\s*sloppatrol-([\w-]+):\s*(.*?)\s*(?:-->|\*/)?$", line
                ):
                    if match[1] not in {"scope", "fix", "inputs"} or match[1] in headers:
                        raise GateError(f"{path}: unknown or duplicate sloppatrol-{match[1]} header")
                    headers[match[1]] = match[2]
            cases = sorted((sloppatrol / "script-tests" / rule_id).glob("*"))
            if not lines or not lines[0].startswith("#!") or not headers.get("fix"):
                raise GateError(f"{path}: shebang and sloppatrol-fix required")
            if not cases or any(not (c / "head").is_dir() or not (c / "expected").is_file() for c in cases):
                raise GateError(f"{path}: each script case needs head/ and expected")
            if {bool((c / "expected").read_text().strip()) for c in cases} != {False, True}:
                raise GateError(f"{path}: script needs nonempty and empty expected cases")
            data = dict(kind="script", scope=headers.get("scope", "all"), note=headers["fix"], cases=cases)
            if "inputs" in headers:  # only --fast reads it: a rule whose inputs did not change cannot move
                data["inputs"] = headers["inputs"].split()
                if not data["inputs"] or not all(
                    any(glob_matches(g, p) for p in paths) for g in data["inputs"]
                ):
                    raise GateError(f"{path}: every sloppatrol-inputs glob must match a corpus path")
        if data["scope"] not in {"all", "changed"}:
            raise GateError(f"{path}: sloppatrol-scope must be all or changed")
        rules[rule_id] = dict(data, path=path)
    for test in (sloppatrol / "rule-tests").glob("*"):
        fields = yaml_fields(test, {"id"}) if test.is_file() else {}
        rule_id = fields.get("id")
        if rule_id not in rules or rules[rule_id]["kind"] != "ast-grep" or test.name != f"{rule_id}-test.yml":
            raise GateError(f"{test}: unknown id or unexpected rule-test filename")
    return rules, ids


def changes(root: Path, base: str, env: dict[str, str] | None = None) -> list[list[str]]:
    rows, tokens = [], iter(ls_z(root, "diff", "--name-status", "-M25%", base, "--", env=env))
    for status in tokens:  # a typechange (T) keeps its path, so it is a modification
        rows.append(
            [status[0].replace("T", "M"), next(tokens)] + ([next(tokens)] if status[0] == "R" else [])
        )
    untracked = ls_z(root, "ls-files", "--others", "--exclude-standard", env=env)
    rows.extend(["A", p] for p in untracked if (root / p).is_file())
    # Script-test cases are outside the corpus (see `corpus`), so they are no change a rule can report.
    rows = [["A", row[2]] if row[0] == "R" and SCRIPT_TEST_CASE.search(row[1]) else row for row in rows]
    return sorted(
        row for row in rows if row[0] in {"A", "M", "D", "R"} and not SCRIPT_TEST_CASE.search(row[-1])
    )


def resolve_base(root: Path, base: dict[str, Any], ref: str | None, all_files: bool) -> None:
    if all_files:
        ref = EMPTY_TREE
    elif ref is None:
        ref = (
            "origin/" + os.environ["GITHUB_BASE_REF"] if os.environ.get("GITHUB_BASE_REF") else "origin/HEAD"
        )
    base["ref"] = ref
    try:
        merge_base = EMPTY_TREE
        if not all_files:  # the empty tree needs no history, so only a real base cares about shallowness
            if git(root, "rev-parse", "--is-shallow-repository") == "true":
                raise GateError("shallow repository")
            git(root, "rev-parse", "--verify", ref + "^{commit}")
            merge_base = git(root, "merge-base", "HEAD", ref)
            head = git(root, "rev-parse", "HEAD")
            if os.environ.get("GITHUB_EVENT_NAME") in PR_EVENTS and merge_base == head:
                raise GateError("merge-base equals HEAD in PR context")
        base.update(merge_base=merge_base, changed=changes(root, merge_base))
    except GateError as error:
        raise GateError(f"{ref}: {error}; set --base and fetch-depth: 0", "sloppatrol-base") from error


def finding(rule_id: str, kind: str, path: str, line: int, message: str) -> dict[str, Any]:
    return dict(
        id=rule_id, kind=kind, path=path, line=line, message=message, suppressed=False, marker_line=None
    )


def script_hits(
    root: Path, script: Path, paths: list[str], base: dict[str, Any], env: dict[str, str] | None = None
) -> list[dict[str, Any]]:
    rule_id, lib, corpus_set = script.stem, script.parent.parent / "lib", set(paths)
    with tempfile.TemporaryDirectory(prefix="sloppatrol-protocol-") as temporary:
        files, changed = Path(temporary) / "files", Path(temporary) / "changed"
        files.write_text("".join(p + "\n" for p in paths))
        env = {
            k: v for k, v in (env or os.environ).items() if k not in {"SLOPPATROL_BASE", "SLOPPATROL_CHANGED"}
        }
        env.update(
            SLOPPATROL_ROOT=str(root),
            SLOPPATROL_RULE=rule_id,
            SLOPPATROL_FILES=str(files),
            SLOPPATROL_LIB=str(lib),
        )
        if base["merge_base"]:
            changed.write_text("".join("\t".join(row) + "\n" for row in base["changed"]))
            env.update(SLOPPATROL_BASE=base["merge_base"], SLOPPATROL_CHANGED=str(changed))
        try:
            result = run([str(script)], root, env=env, timeout=SCRIPT_TIMEOUT)
        except (OSError, UnicodeError, subprocess.TimeoutExpired) as error:
            raise GateError(f"{rule_id}: {error}", "sloppatrol-script-protocol") from error
        hits = []
        for line in result.stdout.splitlines():
            match = re.fullmatch(r"(.+?):([1-9][0-9]*): (\S+) (\S.*)", line)
            if not match or match[1] not in corpus_set or match[3] != rule_id:
                raise GateError(
                    f"{rule_id}: malformed or foreign output: {line}", "sloppatrol-script-protocol"
                )
            hits.append(finding(rule_id, "script", match[1], int(match[2]), match[4]))
        if result.returncode != int(bool(hits)):
            raise GateError(
                f"{rule_id}: exit {result.returncode} with {len(hits)} lines", "sloppatrol-script-protocol"
            )
        return hits


def filter_hits(  # noqa: PLR0913 -- explicit inputs shared by live scans and fixture tests
    root: Path,
    paths: list[str],
    hits: list[dict[str, Any]],
    ids: set[str],
    whole: set[str],
    *,
    selected: set[str] | None = None,
) -> list[dict[str, Any]]:
    """Suppress covered hits in place; return the marker findings. Id-less marker errors always count."""
    raw: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for hit in hits:
        if hit["kind"] != "gate":
            raw[hit["path"]].append(hit)
    checks = []
    for path in paths:
        text = (root / path).read_text(errors="replace")
        for marker in scan_markers(path, text, ids):
            if selected is not None and marker["id"] is not None and marker["id"] not in selected:
                continue
            error = marker["error"]
            if error is None:
                covered = [
                    h
                    for h in raw[path]
                    if h["id"] == marker["id"] and marker_covers(path, text, marker["line"], h["line"])
                ]
                for hit in covered:
                    if not hit["suppressed"]:
                        hit.update(suppressed=True, marker_line=marker["line"])
                if not covered and marker["id"] in whole:
                    error = "sloppatrol-unused-marker"
            if error:
                checks.append(finding(error, "gate", path, marker["line"], marker["id"] or "invalid marker"))
    return checks


def script_tests(root: Path, rule_id: str, data: dict[str, Any], ids: set[str]) -> list[dict[str, Any]]:
    # A git hook exports repository-local variables (GIT_INDEX_FILE, GIT_DIR, ...) and the user's
    # global config may add hooks or excludes: none of them may reach a fixture repository.
    local = set(git(root, "rev-parse", "--local-env-vars").split())
    env = {k: v for k, v in os.environ.items() if k not in local}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
    config = [
        "-c",
        "user.name=sloppatrol",
        "-c",
        "user.email=sloppatrol@example.invalid",
        "-c",
        "commit.gpgsign=false",
    ]
    findings = []
    for case in data["cases"]:
        with tempfile.TemporaryDirectory(prefix="sloppatrol-case-") as temporary:
            work = Path(temporary)
            git(work, "init", "-q", env=env)
            base = EMPTY_TREE
            for tree in (case / "base", case / "head"):
                if not tree.exists():
                    continue
                for entry in work.iterdir():
                    if entry.name != ".git":
                        shutil.rmtree(entry) if entry.is_dir() else entry.unlink()
                shutil.copytree(tree, work, dirs_exist_ok=True)
                git(work, "add", "-A", env=env)
                git(work, *config, "commit", "-qm", "fixture", "--allow-empty", "--no-verify", env=env)
                if tree.name == "base":
                    base = git(work, "rev-parse", "HEAD", env=env)
            paths = corpus(work, env)
            hits = script_hits(
                work, data["path"], paths, dict(merge_base=base, changed=changes(work, base, env)), env
            )
            filter_hits(work, paths, hits, ids, set())
            actual = {f"{h['path']}:{h['line']}" for h in hits if not h["suppressed"]}
            expected = {line.strip() for line in (case / "expected").read_text().splitlines() if line.strip()}
            if actual != expected:
                difference = f"missing {sorted(expected - actual)}, extra {sorted(actual - expected)}"
                findings.append(
                    finding("sloppatrol-script-test", "gate", "", 0, f"{rule_id}/{case.name}: {difference}")
                )
    return findings


def ast_run(root: Path, args: list[str], allowed: set[int]) -> subprocess.CompletedProcess[str]:
    result = run(["ast-grep", *args], root)
    if result.returncode not in allowed or re.search(r"(?m)^ERROR", result.stderr):
        raise GateError(f"ast-grep exit {result.returncode}: {result.stderr.strip()}", "sloppatrol-ast-grep")
    return result


def ast_grep_reads(path: Path) -> bool:
    """Mirror ast-grep's `read_file`: skip empty files and those over 3 MB and 200,000 lines."""
    size = path.stat().st_size
    if not size:
        return False
    if size <= 3_000_000:
        return True
    with path.open("rb") as handle:
        return sum(1 for _ in handle) <= 200_000


def ast_scan(
    root: Path, rules: dict[str, Any], paths: list[str], filters: list[str], changed: set[str]
) -> list[dict[str, Any]]:
    """Scan in chunks; fail closed unless every selected rule loaded and every source file was read."""
    paths = [p for p in paths if ast_grep_reads(root / p)]
    hits, parsed, corpus_set = [], set(), set(paths)
    for offset in range(0, len(paths), 1000):
        chunk = paths[offset : offset + 1000]
        scan = ["scan", "-c", CONFIG, "--json=stream", "--inspect", "entity", *filters, "--", *chunk]
        result = ast_run(root, scan, {0, 1})
        counts = dict(re.findall(r"(effectiveRuleCount|skippedFileCount)=(\d+)", result.stderr))
        if counts != dict(effectiveRuleCount=str(len(rules)), skippedFileCount="0"):
            raise GateError(
                f"ast-grep loaded {counts.get('effectiveRuleCount', '?')} of {len(rules)} rules and "
                f"could not read {counts.get('skippedFileCount', '?')} files in {chunk[0]} .. {chunk[-1]}; "
                f"check {CONFIG} and that source files are UTF-8",
                "sloppatrol-ast-grep",
            )
        parsed.update(
            re.findall(r"(?m)^sg: entity\|file\|(.+): language=\w+,appliedRuleCount=[1-9]", result.stderr)
        )
        for line in result.stdout.splitlines():
            try:
                hit = json.loads(line)
                rule_id, path, message = hit["ruleId"], hit["file"], hit["message"]
                if rule_id not in rules or path not in corpus_set or not isinstance(message, str):
                    raise ValueError("invalid hit")
                if rules[rule_id]["scope"] == "all" or path in changed:
                    hits.append(
                        finding(rule_id, "ast-grep", path, hit["range"]["start"]["line"] + 1, message)
                    )
            except (ValueError, TypeError, KeyError) as error:
                raise GateError(f"ast-grep invalid JSON hit: {line}", "sloppatrol-ast-grep") from error
    # ast-grep honours its own ignore comment in every file a rule applies to, including languages the
    # comment table lacks (a languageGlobs mapping, say); there the bare word fails closed.
    for path in sorted(p for p in parsed if comment_token(p) is None):
        for number, line in enumerate((root / path).read_text(errors="replace").split("\n"), 1):
            if "ast-grep-ignore" in line:
                hits.append(finding("sloppatrol-ast-grep-ignore", "gate", path, number, "invalid marker"))
    return hits


def rule_filter(selected: dict[str, Any]) -> list[str]:
    return ["--filter", "^(" + "|".join(sorted(selected)) + ")$"]


def edited_rules(root: Path, rules: dict[str, Any], merge_base: str) -> set[str]:
    """Rules whose definition, tests or shared library the branch touched; every rule if the gate changed."""
    touched = set(ls_z(root, "diff", "--name-only", "--no-renames", merge_base, "--", ".sloppatrol"))
    touched |= set(ls_z(root, "ls-files", "--others", "--exclude-standard", "--", ".sloppatrol"))
    if ".sloppatrol/gate.py" in touched:
        return set(rules)
    edited = set()
    for rule_id, data in rules.items():
        own = {data["path"].relative_to(root).as_posix()}
        if data["kind"] == "ast-grep":
            own |= {f".sloppatrol/rule-tests/{rule_id}-test.yml", CONFIG}
            prefixes: tuple[str, ...] = ()
        else:
            prefixes = (f".sloppatrol/script-tests/{rule_id}/", ".sloppatrol/lib/")
        if touched & own or (prefixes and any(p.startswith(prefixes) for p in touched)):
            edited.add(rule_id)
    return edited


def unmoved_scripts(rules: dict[str, Any], edited: set[str], rows: list[list[str]]) -> set[str]:
    """Whole-tree scripts that declare `sloppatrol-inputs` and whose inputs no changed path touches."""
    touched = {path for row in rows for path in row[1:]}
    return {
        rule_id
        for rule_id, data in rules.items()
        if data["kind"] == "script"
        and data["scope"] == "all"
        and "inputs" in data
        and rule_id not in edited
        and not any(glob_matches(g, p) for g in data["inputs"] for p in touched)
    }


def run_script(  # noqa: PLR0913 (one worker's inputs)
    root: Path,
    rule_id: str,
    data: dict[str, Any],
    *,
    ids: set[str],
    base: dict[str, Any],
    paths: list[str],
    test: bool,
) -> list[dict[str, Any]]:
    try:
        tests = script_tests(root, rule_id, data, ids) if test else []
        return [*tests, *script_hits(root, data["path"], paths, base)]
    except GateError as error:
        if error.code != "sloppatrol-script-protocol":
            raise
        return [finding(error.code, "gate", "", 0, str(error))]


def execute(  # noqa: PLR0912 (one branch per gate stage)
    root: Path, args: argparse.Namespace, report: dict[str, Any], rules: dict[str, Any]
) -> set[str]:
    """Fill the report; return the scripts --fast skipped."""
    if not args.rule and not args.fast:
        shallow = run(["git", "rev-parse", "--is-shallow-repository"], root)
        if shallow.returncode == 0 and shallow.stdout.strip() == "true":
            raise GateError("shallow repository; set --base and fetch-depth: 0", "sloppatrol-base")
        report["findings"].extend(retired_findings(root))
    paths = corpus(root)
    discovered, ids = discover(root, paths)
    if args.rule and (unknown := set(args.rule) - discovered.keys()):
        raise GateError(f"unknown rule id: {', '.join(sorted(unknown))}")
    rules.update({k: v for k, v in discovered.items() if not args.rule or k in args.rule})
    ast_rules = {k: v for k, v in rules.items() if v["kind"] == "ast-grep"}
    if ast_rules:
        if not shutil.which("ast-grep"):
            raise GateError("ast-grep is required on PATH when rules exist")
        report["ast_grep"] = ast_run(root, ["--version"], {0}).stdout.strip().removeprefix("ast-grep ")
    if args.all or args.fast or any(v["scope"] == "changed" for v in rules.values()):
        resolve_base(root, report["base"], args.base, args.all)
    base = report["base"]
    # --fast keeps every finding the branch can cause: a rule's own tests run only when the rule changed,
    # an unchanged single-file ast-grep rule scans only changed files, and a script whose declared
    # sloppatrol-inputs the diff misses is skipped. Findings inherited unchanged from the base are not shown.
    edited = edited_rules(root, rules, base["merge_base"]) if args.fast else set(rules)
    skipped = unmoved_scripts(rules, edited, base["changed"]) if args.fast else set()
    changed = {row[-1] for row in base["changed"] if row[0] in {"A", "M", "R"}}
    hits = report["findings"]
    diff_only = {k: v for k, v in ast_rules.items() if k not in edited}
    if tested := {k: v for k, v in ast_rules.items() if k in edited}:
        filters = rule_filter(tested) if args.rule or diff_only else []
        result = ast_run(root, ["test", "-c", CONFIG, "--skip-snapshot-tests", *filters], {0, 4})
        if not re.search(rf"(?m)^Running {len(tested)} tests$", result.stdout):
            raise GateError(
                f"ast-grep test did not run {len(tested)} rule tests; check {CONFIG}", "sloppatrol-ast-grep"
            )
        if result.returncode == 4:
            hits.append(finding("sloppatrol-rule-test", "gate", "", 0, " ".join(result.stdout.split())))
        hits.extend(ast_scan(root, tested, paths, filters, changed))
    if diff_only:
        scanned = [p for p in paths if p in changed]
        hits.extend(ast_scan(root, diff_only, scanned, rule_filter(diff_only), changed))
    scripts = {k: v for k, v in rules.items() if v["kind"] == "script" and k not in skipped}
    with concurrent.futures.ThreadPoolExecutor(max_workers=os.cpu_count()) as pool:
        futures = [
            pool.submit(run_script, root, k, v, ids=ids, base=base, paths=paths, test=k in edited)
            for k, v in scripts.items()
        ]
        for future in futures:
            hits.extend(future.result())
    unique = {(h["id"], h["path"], h["line"], h["message"] if h["kind"] == "gate" else ""): h for h in hits}
    hits[:] = unique.values()
    whole = {k for k, v in rules.items() if (v["scope"] == "all" or args.all) and k not in skipped}
    selected = set(args.rule) if args.rule else None
    if diff_only:  # a marker for an ast-grep rule that did not scan its file cannot be judged unused
        unscanned = [p for p in paths if p not in changed]
        hits.extend(filter_hits(root, unscanned, hits, ids, whole - diff_only.keys(), selected=selected))
        hits.extend(
            filter_hits(root, [p for p in paths if p in changed], hits, ids, whole, selected=selected)
        )
    else:
        hits.extend(filter_hits(root, paths, hits, ids, whole, selected=selected))
    return skipped


def github_escape(value: str, *, property_value: bool = False) -> str:
    value = value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    return value.replace(":", "%3A").replace(",", "%2C") if property_value else value


SUMMARY_BEGIN, SUMMARY_END = "=== sloppatrol findings:", "=== end sloppatrol findings ==="


def github_report(visible: list[dict[str, Any]]) -> None:
    """Annotate each finding, then list them all: a step shows only its first ten error annotations."""
    for hit in visible:
        path = github_escape(hit["path"], property_value=True)
        location = f"file={path},line={hit['line']}," if hit["path"] else ""
        title = github_escape(hit["id"], property_value=True)
        print(f"::error {location}title={title}::{github_escape(' '.join(hit['message'].split()))}")
    if not visible:
        return
    print(f"{SUMMARY_BEGIN} {len(visible)} ===")
    for rule_id in sorted({h["id"] for h in visible}):
        sites = [
            f"{h['path']}:{h['line']}" if h["path"] else "sloppatrol" for h in visible if h["id"] == rule_id
        ]
        print(f"{rule_id} ({len(sites)}): {' '.join(sites)}")
    print(SUMMARY_END)
    sys.stdout.flush()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rule", action="append")
    bases = parser.add_mutually_exclusive_group()
    bases.add_argument("--base")
    bases.add_argument("--all", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--fast", action="store_true", help="only what this branch can change")
    args, started = parser.parse_args(), time.monotonic()
    if args.base == "":
        parser.error("--base needs a ref")
    if args.fast and args.all:
        parser.error("--fast needs a base, not --all")
    rules: dict[str, Any] = {}
    report: dict[str, Any] = dict(
        version=VERSION, ast_grep=None, base=dict(ref=None, merge_base=None, changed=[]), findings=[]
    )
    failed, skipped = False, set()
    try:
        root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel"))
        skipped = execute(root, args, report, rules)
    except (GateError, OSError, ValueError) as error:
        failed = True
        report["findings"].append(
            finding(
                error.code if isinstance(error, GateError) else "sloppatrol-config", "gate", "", 0, str(error)
            )
        )
    hits = sorted(report["findings"], key=lambda h: (h["path"], h["line"], h["id"], h["message"]))
    report["findings"] = hits
    base = report["base"]
    base["changed"] = len(base["changed"])
    visible = [h for h in hits if not h["suppressed"]]
    if args.json:
        print(json.dumps(report))
    else:
        for hit in visible:
            location = f"{hit['path']}:{hit['line']}:" if hit["path"] else "sloppatrol:"
            print(f"{location} {hit['id']} {' '.join(hit['message'].split())}")
        fixes = sorted({h["id"] for h in visible} & rules.keys())
        if fixes:
            print("fix:")
            for rule_id in fixes:
                print(f"  {rule_id}: {' '.join(rules[rule_id]['note'].split())}")
        if os.environ.get("GITHUB_ACTIONS") == "true":
            github_report(visible)
    ast_count = sum(v["kind"] == "ast-grep" for v in rules.values())
    print(
        f"sloppatrol gate {VERSION}: {len(visible)} findings, {len(hits) - len(visible)} suppressed, "
        f"{len(rules)} rules ({ast_count} ast-grep, {len(rules) - ast_count} script); "
        f"ast-grep {report['ast_grep'] or 'not needed'}; "
        + (
            f"base {base['ref']} (merge-base {(base['merge_base'] or '-')[:12]}), "
            f"{base['changed']} changed files; "
            if base["ref"]
            else "base not needed, every rule scans all files; "
        )
        + (f"--fast: {len(skipped)} scripts skipped; " if args.fast else "")
        + f"{time.monotonic() - started:.1f} s",
        file=sys.stderr,
    )
    return 2 if failed or any(h["id"] == "sloppatrol-script-protocol" for h in hits) else int(bool(visible))


if __name__ == "__main__":
    sys.exit(main())
