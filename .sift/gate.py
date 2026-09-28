#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Agent Labs
# SPDX-License-Identifier: MIT
"""Run a repository's rules and tests; filter their hits through shared markers."""

from __future__ import annotations

import argparse
import fnmatch
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
VERSION = "0.3.0"
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
CONFIG = ".sift/sgconfig.yml"
SCRIPT_TIMEOUT = 600.0
BUILTIN_LENSES: frozenset[str] = frozenset(
    "comment-narration copy-slop dead-code defensive-noise oversized-modules parallel-implementations "  # noqa: SIM905 (a word list reads best as one string)
    "reinvented-wheel session-residue silent-fallbacks speculative-abstraction stale-docs "
    "standards stringly-typed test-plumbing wall-of-text".split()
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


MARKER_START = r"(?<![\w./-])sift:"


def scan_markers(path: str, text: str, ids: set[str] | frozenset[str]) -> list[dict[str, Any]]:
    """Return {line, id, reason, error}; comment-shaped multiline-string lines count as comments."""
    markers = []
    for number, line in enumerate(text.split("\n"), 1):
        parsed = comment(path, line)
        if parsed is None:
            continue
        body, whole = parsed
        if "ast-grep-ignore" in body:
            markers.append(dict(line=number, id=None, reason="", error="sift-ast-grep-ignore"))
        # The marker word inside a path or another word, such as a .sift directory, is prose.
        if not (named := re.search(rf"{MARKER_START}\s*({ID})?", body)):
            continue
        match = re.fullmatch(rf"sift: ({ID}) -(?: (.*))?", body)
        rule_id, reason = (match[1], match[2] or "") if match else (named[1], "")
        words = re.findall(r"[^\W_]+", reason.casefold())
        error = None
        if not whole:
            error = "sift-trailing-marker"
        elif not match:
            error = "sift-bad-marker"
        elif rule_id not in ids or rule_id.startswith("sift-"):
            error = "sift-unknown-id"
        elif len(words) < 3 or " ".join(words) in GENERIC_REASONS:
            error = "sift-weak-reason"
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
        return re.fullmatch("".join(parts), path) is not None
    except re.error as error:
        raise GateError(f"invalid files glob {pattern}: {error}") from error


class GateError(Exception):
    def __init__(self, message: str, code: str = "sift-config"):
        super().__init__(message)
        self.code = code


def run(args: list[str], root: Path, **kwargs: Any) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=root, capture_output=True, text=True, check=False, **kwargs)


def git(root: Path, *args: str, env: dict[str, str] | None = None) -> str:
    result = run(["git", *args], root, env=env)
    if result.returncode:
        raise GateError(result.stderr.strip())
    return result.stdout.strip("\n")


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
                scope = [line.strip() for line in body if re.match(r"\s+sift-scope\s*:", line)]
                if len(scope) > 1:
                    raise ValueError("duplicate sift-scope")
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
    """Script-test cases are inputs, wherever they live: `.sift/`, a rule catalog or a standards pack."""
    paths = ls_z(root, "ls-files", "-co", "--exclude-standard", env=env)
    return sorted({p for p in paths if not SCRIPT_TEST_CASE.search(p) and (root / p).is_file()})


def discover(root: Path, paths: list[str]) -> tuple[dict[str, Any], set[str]]:  # noqa: PLR0912 (one branch per layout check)
    rules, ids = {}, set(BUILTIN_LENSES)
    sift = root / ".sift"
    for path in sorted(
        [*(sift / "rules").glob("*.yml"), *(sift / "scripts").glob("*"), *(sift / "lenses").glob("*.md")]
    ):
        rule_id = path.stem
        if not re.fullmatch(ID, rule_id) or rule_id.startswith("sift-") or rule_id in ids:
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
            test = sift / "rule-tests" / f"{rule_id}-test.yml"
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
                    r"\s*(?:#|//|--|;|%|<!--|/\*)\s*sift-([\w-]+):\s*(.*?)\s*(?:-->|\*/)?$", line
                ):
                    if match[1] not in {"scope", "fix"} or match[1] in headers:
                        raise GateError(f"{path}: unknown or duplicate sift-{match[1]} header")
                    headers[match[1]] = match[2]
            cases = sorted((sift / "script-tests" / rule_id).glob("*"))
            if not lines or not lines[0].startswith("#!") or not headers.get("fix"):
                raise GateError(f"{path}: shebang and sift-fix required")
            if not cases or any(not (c / "head").is_dir() or not (c / "expected").is_file() for c in cases):
                raise GateError(f"{path}: each script case needs head/ and expected")
            if {bool((c / "expected").read_text().strip()) for c in cases} != {False, True}:
                raise GateError(f"{path}: script needs nonempty and empty expected cases")
            data = dict(kind="script", scope=headers.get("scope", "all"), note=headers["fix"], cases=cases)
        if data["scope"] not in {"all", "changed"}:
            raise GateError(f"{path}: sift-scope must be all or changed")
        rules[rule_id] = dict(data, path=path)
    for test in (sift / "rule-tests").glob("*"):
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
        raise GateError(f"{ref}: {error}; set --base and fetch-depth: 0", "sift-base") from error


def finding(rule_id: str, kind: str, path: str, line: int, message: str) -> dict[str, Any]:
    return dict(
        id=rule_id, kind=kind, path=path, line=line, message=message, suppressed=False, marker_line=None
    )


def script_hits(
    root: Path, script: Path, paths: list[str], base: dict[str, Any], env: dict[str, str] | None = None
) -> list[dict[str, Any]]:
    rule_id, lib, corpus_set = script.stem, script.parent.parent / "lib", set(paths)
    with tempfile.TemporaryDirectory(prefix="sift-protocol-") as temporary:
        files, changed = Path(temporary) / "files", Path(temporary) / "changed"
        files.write_text("".join(p + "\n" for p in paths))
        env = {k: v for k, v in (env or os.environ).items() if k not in {"SIFT_BASE", "SIFT_CHANGED"}}
        env.update(SIFT_ROOT=str(root), SIFT_RULE=rule_id, SIFT_FILES=str(files), SIFT_LIB=str(lib))
        if base["merge_base"]:
            changed.write_text("".join("\t".join(row) + "\n" for row in base["changed"]))
            env.update(SIFT_BASE=base["merge_base"], SIFT_CHANGED=str(changed))
        try:
            result = run([str(script)], root, env=env, timeout=SCRIPT_TIMEOUT)
        except (OSError, UnicodeError, subprocess.TimeoutExpired) as error:
            raise GateError(f"{rule_id}: {error}", "sift-script-protocol") from error
        hits = []
        for line in result.stdout.splitlines():
            match = re.fullmatch(r"(.+?):([1-9][0-9]*): (\S+) (\S.*)", line)
            if not match or match[1] not in corpus_set or match[3] != rule_id:
                raise GateError(f"{rule_id}: malformed or foreign output: {line}", "sift-script-protocol")
            hits.append(finding(rule_id, "script", match[1], int(match[2]), match[4]))
        if result.returncode != int(bool(hits)):
            raise GateError(
                f"{rule_id}: exit {result.returncode} with {len(hits)} lines", "sift-script-protocol"
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
                    error = "sift-unused-marker"
            if error:
                checks.append(finding(error, "gate", path, marker["line"], marker["id"] or "invalid marker"))
    return checks


def script_tests(root: Path, rule_id: str, data: dict[str, Any], ids: set[str]) -> list[dict[str, Any]]:
    # A git hook exports repository-local variables (GIT_INDEX_FILE, GIT_DIR, ...) and the user's
    # global config may add hooks or excludes: none of them may reach a fixture repository.
    local = set(git(root, "rev-parse", "--local-env-vars").split())
    env = {k: v for k, v in os.environ.items() if k not in local}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
    config = ["-c", "user.name=sift", "-c", "user.email=sift@example.invalid", "-c", "commit.gpgsign=false"]
    findings = []
    for case in data["cases"]:
        with tempfile.TemporaryDirectory(prefix="sift-case-") as temporary:
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
                    finding("sift-script-test", "gate", "", 0, f"{rule_id}/{case.name}: {difference}")
                )
    return findings


def ast_run(root: Path, args: list[str], allowed: set[int]) -> subprocess.CompletedProcess[str]:
    result = run(["ast-grep", *args], root)
    if result.returncode not in allowed or re.search(r"(?m)^ERROR", result.stderr):
        raise GateError(f"ast-grep exit {result.returncode}: {result.stderr.strip()}", "sift-ast-grep")
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
                "sift-ast-grep",
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
                raise GateError(f"ast-grep invalid JSON hit: {line}", "sift-ast-grep") from error
    # ast-grep honours its own ignore comment in every file a rule applies to, including languages the
    # comment table lacks (a languageGlobs mapping, say); there the bare word fails closed.
    for path in sorted(p for p in parsed if comment_token(p) is None):
        for number, line in enumerate((root / path).read_text(errors="replace").split("\n"), 1):
            if "ast-grep-ignore" in line:
                hits.append(finding("sift-ast-grep-ignore", "gate", path, number, "invalid marker"))
    return hits


def execute(root: Path, args: argparse.Namespace, report: dict[str, Any], rules: dict[str, Any]) -> None:
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
    if args.all or any(v["scope"] == "changed" for v in rules.values()):
        resolve_base(root, report["base"], args.base, args.all)
    hits = report["findings"]
    if ast_rules:
        filters = ["--filter", "^(" + "|".join(sorted(ast_rules)) + ")$"] if args.rule else []
        result = ast_run(root, ["test", "-c", CONFIG, "--skip-snapshot-tests", *filters], {0, 4})
        if not re.search(rf"(?m)^Running {len(ast_rules)} tests$", result.stdout):
            raise GateError(
                f"ast-grep test did not run {len(ast_rules)} rule tests; check {CONFIG}", "sift-ast-grep"
            )
        if result.returncode == 4:
            hits.append(finding("sift-rule-test", "gate", "", 0, " ".join(result.stdout.split())))
        changed = {row[-1] for row in report["base"]["changed"] if row[0] in {"A", "M", "R"}}
        hits.extend(ast_scan(root, ast_rules, paths, filters, changed))
    for rule_id, data in rules.items():
        if data["kind"] == "script":
            try:
                hits.extend(script_tests(root, rule_id, data, ids))
                hits.extend(script_hits(root, data["path"], paths, report["base"]))
            except GateError as error:
                if error.code != "sift-script-protocol":
                    raise
                hits.append(finding(error.code, "gate", "", 0, str(error)))
    unique = {(h["id"], h["path"], h["line"], h["message"] if h["kind"] == "gate" else ""): h for h in hits}
    hits[:] = unique.values()
    whole = {k for k, v in rules.items() if v["scope"] == "all" or args.all}
    hits.extend(filter_hits(root, paths, hits, ids, whole, selected=set(args.rule) if args.rule else None))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rule", action="append")
    bases = parser.add_mutually_exclusive_group()
    bases.add_argument("--base")
    bases.add_argument("--all", action="store_true")
    parser.add_argument("--json", action="store_true")
    args, started = parser.parse_args(), time.monotonic()
    if args.base == "":
        parser.error("--base needs a ref")
    rules: dict[str, Any] = {}
    report: dict[str, Any] = dict(
        version=VERSION, ast_grep=None, base=dict(ref=None, merge_base=None, changed=[]), findings=[]
    )
    failed = False
    try:
        root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel"))
        execute(root, args, report, rules)
    except (GateError, OSError, ValueError) as error:
        failed = True
        report["findings"].append(
            finding(error.code if isinstance(error, GateError) else "sift-config", "gate", "", 0, str(error))
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
            location = f"{hit['path']}:{hit['line']}:" if hit["path"] else "sift:"
            print(f"{location} {hit['id']} {' '.join(hit['message'].split())}")
        fixes = sorted({h["id"] for h in visible} & rules.keys())
        if fixes:
            print("fix:")
            for rule_id in fixes:
                print(f"  {rule_id}: {' '.join(rules[rule_id]['note'].split())}")
    ast_count = sum(v["kind"] == "ast-grep" for v in rules.values())
    print(
        f"sift gate {VERSION}: {len(visible)} findings, {len(hits) - len(visible)} suppressed, "
        f"{len(rules)} rules ({ast_count} ast-grep, {len(rules) - ast_count} script); "
        f"ast-grep {report['ast_grep'] or 'not needed'}; base {base['ref'] or 'not needed'} "
        f"(merge-base {(base['merge_base'] or '-')[:12]}), {base['changed']} changed files; "
        f"{time.monotonic() - started:.1f} s",
        file=sys.stderr,
    )
    return 2 if failed or any(h["id"] == "sift-script-protocol" for h in hits) else int(bool(visible))


if __name__ == "__main__":
    sys.exit(main())
