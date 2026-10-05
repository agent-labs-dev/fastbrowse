"""Independent grading for the external corpus: a trusted local command, and WindTunnel's own answer predicates.

The command is one the caller deliberately installed and selected. Its file digest is recorded and re-checked, so
a grade names the code that produced it. The bridge is JSON on stdio, with no shell, a bounded timeout and a
process group killed on timeout or cancellation; the environment is an allowlist, so provider keys never reach it.
A command that times out or prints anything but a valid `Grade` raises `GraderError`, leaving the attempt
ungraded. An action predicate with no command raises `UnsupportedPredicate` for the same reason: it needs a real
state probe, which only a supplied evaluator can provide.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
import math
import os
import re
import shutil
import signal
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Literal

from pydantic import BaseModel, ConfigDict, JsonValue, TypeAdapter, ValidationError

from fastbrowse.evals.corpus import Grade, TaskRef
from fastbrowse.evals.live_tasks import Outcome

if TYPE_CHECKING:
    from fastbrowse.evals.corpus import Grader

DEFAULT_TIMEOUT = 60.0
"""Seconds one grader command may run before it is killed and the attempt is left ungraded."""

SAFE_ENVIRONMENT = ("HOME", "LANG", "LC_ALL", "LC_CTYPE", "PATH", "TMPDIR", "TZ")
"""The only variables the grader inherits. A key absent here never reaches the command, which is how provider
keys and remote credentials stay out of a process we did not write."""

_HEX_DIGEST = re.compile(r"[a-f0-9]{64}")
_DASHES = re.compile("[\u2010-\u2015\u2212]")
_APOSTROPHES = re.compile("[\u2018\u2019`\u00b4]")
_URL = re.compile(r"https?://\S+")
_NUMBER_RANGE = re.compile(r"(\d)\s+to\s+(\d)")
_SEPARATORS = re.compile(r"[-_]+")
_SPACE = re.compile(r"\s+")

NATIVE_GRADER = "windtunnel-answer"
NATIVE_VERSION = "1"


class GraderError(RuntimeError):
    """The trusted grader did not produce a grade. The corpus keeps the attempt ungraded."""


class UnsupportedPredicate(GraderError):
    """This predicate needs a live state probe, which no native answer grader can supply."""


def safe_environment(source: Mapping[str, str] | None = None) -> dict[str, str]:
    """The allowlisted slice of an environment, for a command that must not see provider keys."""
    origin = os.environ if source is None else source
    return {name: origin[name] for name in SAFE_ENVIRONMENT if name in origin}


def normalize_answer(value: str) -> str:
    """The pinned predicates' own normalisation, kept in step with `predicates.mjs`.

    Typographic dashes, prose ranges and curly apostrophes are folded before comparison, because an agent
    restates site text with the glyph it copied rather than the one the predicate author typed.
    """
    text = value.lower()
    text = _URL.sub(" ", text)
    text = _DASHES.sub("-", text)
    text = _APOSTROPHES.sub("'", text)
    text = _NUMBER_RANGE.sub(r"\1-\2", text)
    text = _SEPARATORS.sub(" ", text)
    text = _SPACE.sub(" ", text)
    return text.strip()


class _AnswerPredicate(BaseModel):
    """The answer half of a WindTunnel predicate. An unknown key is refused, not ignored, because silently
    dropping a term the upstream author wrote would let a wrong answer pass."""

    model_config = ConfigDict(extra="forbid")

    type: Literal["answer"]
    contains: tuple[str, ...] = ()
    contains_any: tuple[str, ...] = ()
    matches: str | None = None
    not_contains: tuple[str, ...] = ()


def _predicate(ref: TaskRef) -> dict[str, JsonValue] | None:
    value = ref.metadata.get("predicate")
    return value if isinstance(value, dict) else None


def _predicate_digest(raw: Mapping[str, JsonValue]) -> str:
    body = json.dumps(raw, sort_keys=True, separators=(",", ":"), default=str)
    return hashlib.sha256(body.encode()).hexdigest()[:16]


def grade_answer_predicate(ref: TaskRef, outcome: Outcome) -> Grade:
    """Score a WindTunnel answer predicate against the agent's own final text, in normalised form.

    The predicate is pinned upstream data, not agent output, so this is an independent judgement. An action
    predicate, or an answer predicate with no expected text, is not something this grader can judge and is
    raised as ungraded rather than counted as a failure.
    """
    raw = _predicate(ref)
    if raw is None:
        raise UnsupportedPredicate(f"{ref.id}: task carries no predicate to score")
    if raw.get("type") != "answer":
        raise UnsupportedPredicate(f"{ref.id}: predicate {raw.get('type')!r} needs a live state probe")
    try:
        predicate = _AnswerPredicate.model_validate(raw)
    except ValidationError as exc:
        raise GraderError(f"{ref.id}: malformed answer predicate: {exc}") from exc
    if not predicate.contains and not predicate.contains_any and not predicate.matches:
        raise GraderError(f"{ref.id}: answer predicate has no expected text")
    text = normalize_answer(outcome.answer or "")
    wanted = raw
    passed = all(normalize_answer(term) in text for term in predicate.contains)
    if predicate.contains_any:
        passed = passed and any(normalize_answer(term) in text for term in predicate.contains_any)
    if predicate.matches:
        try:
            passed = passed and re.search(predicate.matches, outcome.answer or "", re.IGNORECASE) is not None
        except re.error as exc:
            raise GraderError(f"{ref.id}: answer predicate has an invalid pattern: {exc}") from exc
    failure: str | None = None
    if not passed:
        failure = f"answer predicate failed: {json.dumps(wanted, sort_keys=True)}"
    else:
        forbidden = next((term for term in predicate.not_contains if normalize_answer(term) in text), None)
        if forbidden is not None:
            failure = f"answer predicate failed (negation): {forbidden}"
    return Grade(
        grader=NATIVE_GRADER,
        version=f"{NATIVE_VERSION}+{_predicate_digest(wanted)}",
        passed=failure is None,
        failure=failure,
        evidence={"predicate": dict(wanted)},
    )


class _OutcomePayload(BaseModel):
    """The observed attempt as the bridge sees it: the harness's reading, never the agent's status."""

    model_config = ConfigDict(extra="forbid")

    answer: str | None = None
    data: JsonValue = None
    final_url: str | None = None
    quotes: tuple[tuple[str, str], ...] | None = None
    controls: tuple[tuple[str, str | None], ...] | None = None
    evidence: JsonValue = None
    unobservable: bool = False


class _Request(BaseModel):
    model_config = ConfigDict(extra="forbid")

    schema_version: Literal[1] = 1
    task: TaskRef
    outcome: _OutcomePayload


def _jsonable(value: object) -> JsonValue:
    if isinstance(value, BaseModel):
        value = value.model_dump(mode="json")
    elif isinstance(value, Mapping):
        value = {str(key): _jsonable(item) for key, item in value.items()}
    elif isinstance(value, (list, tuple)):
        value = [_jsonable(item) for item in value]
    try:
        return TypeAdapter(JsonValue).validate_python(value)
    except ValidationError as exc:
        raise GraderError(f"attempt value is not representable as JSON: {exc}") from exc


def grade_request(ref: TaskRef, outcome: Outcome) -> _Request:
    """The one JSON request a grader command receives: the pinned task and the observed attempt."""
    return _Request(
        task=ref,
        outcome=_OutcomePayload(
            answer=outcome.answer,
            data=_jsonable(outcome.data),
            final_url=outcome.final_url,
            quotes=outcome.quotes,
            controls=outcome.controls,
            evidence=None if outcome.evidence is None else _jsonable(outcome.evidence),
            unobservable=outcome.unobservable,
        ),
    )


def _resolve_code(command: Sequence[str], code: str | Path | None) -> Path:
    if code is None and len(command) > 1:
        raise ValueError("grader code must be explicit when the command has arguments")
    if code is not None:
        path = Path(code)
    else:
        found = shutil.which(command[0])
        if found is None:
            raise ValueError(f"grader command not found: {command[0]}")
        path = Path(found)
    path = path.resolve()
    if not path.is_file():
        raise ValueError(f"grader code is not a file: {path}")
    return path


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _kill(process: asyncio.subprocess.Process) -> None:
    # start_new_session makes the child its own group leader, so its pid is the group id. Signal the group even
    # when the leader has already exited, or a child it spawned would survive the timeout.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (AttributeError, ProcessLookupError, PermissionError):
        with contextlib.suppress(ProcessLookupError):
            process.kill()


async def _terminate(process: asyncio.subprocess.Process) -> None:
    _kill(process)
    with contextlib.suppress(asyncio.CancelledError, ProcessLookupError):
        await process.wait()


async def _run_command(
    command: Sequence[str], payload: bytes, *, seconds: float, environment: Mapping[str, str]
) -> bytes:
    try:
        process = await asyncio.create_subprocess_exec(
            *command,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=dict(environment),
            start_new_session=True,
        )
    except FileNotFoundError as exc:
        raise GraderError(f"grader command not found: {command[0]}") from exc
    except OSError as exc:
        raise GraderError(f"grader command could not start: {exc}") from exc
    try:
        async with asyncio.timeout(seconds):
            stdout, stderr = await process.communicate(payload)
    except TimeoutError as exc:
        await _terminate(process)
        raise GraderError(f"grader command exceeded {seconds:g}s and was terminated") from exc
    except BaseException:
        await _terminate(process)
        raise
    if process.returncode != 0:
        detail = stderr.decode("utf-8", errors="replace").strip()[:500]
        raise GraderError(f"grader command exited {process.returncode}: {detail}")
    return stdout


class LocalCommandGrader:
    """A deliberately installed evaluator behind a JSON stdio bridge, pinned by the digest of its code."""

    def __init__(
        self,
        command: Sequence[str],
        *,
        digest: str | None = None,
        code: str | Path | None = None,
        timeout: float = DEFAULT_TIMEOUT,
    ) -> None:
        command = tuple(command)
        if not command:
            raise ValueError("grader command must not be empty")
        if any(not isinstance(part, str) or not part for part in command):
            raise ValueError("grader command must be non-empty strings")
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("grader timeout must be a positive number of seconds")
        self._command = command
        self._code = _resolve_code(command, code)
        self._digest = file_digest(self._code)
        if digest is not None:
            if _HEX_DIGEST.fullmatch(digest) is None:
                raise ValueError("grader digest must be a lowercase sha256 hex string")
            if digest != self._digest:
                raise ValueError(f"grader code digest does not match the pinned digest: {self._digest} != {digest}")
        self._timeout = float(timeout)

    @property
    def digest(self) -> str:
        return self._digest

    async def __call__(self, ref: TaskRef, outcome: Outcome) -> Grade:
        # Re-read the code on every call: an evaluator edited after it was pinned must not keep grading under the
        # old digest, which would let a changed evaluator pass as the reviewed one.
        current = file_digest(self._code)
        if current != self._digest:
            raise GraderError(f"grader code changed since it was pinned: {current} != {self._digest}")
        payload = grade_request(ref, outcome).model_dump_json().encode("utf-8")
        stdout = await _run_command(self._command, payload, seconds=self._timeout, environment=safe_environment())
        try:
            reported = Grade.model_validate_json(stdout)
        except ValidationError as exc:
            raise GraderError(f"grader output is not a valid Grade: {exc}") from exc
        evidence = dict(reported.evidence)
        evidence.update(
            {
                "grader_command": list(self._command),
                "grader_code": str(self._code),
                "grader_code_sha256": self._digest,
                "reported_grader": reported.grader,
                "reported_version": reported.version,
            }
        )
        return Grade(
            grader=reported.grader,
            version=f"{reported.version}+{self._digest[:16]}",
            passed=reported.passed,
            failure=reported.failure,
            evidence=evidence,
        )


def local_command_grader(
    command: Sequence[str],
    *,
    digest: str | None = None,
    code: str | Path | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> Grader:
    """The corpus `Grader` over an explicitly selected local command. `code` names the file whose digest pins the
    grader; a command with arguments requires it explicitly. A supplied `digest` must match that file."""
    return LocalCommandGrader(command, digest=digest, code=code, timeout=timeout)


def predicate_grader(
    command: Sequence[str] | None = None,
    *,
    digest: str | None = None,
    code: str | Path | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> Grader:
    """Route a WindTunnel answer predicate through the native scorer; send everything else to the local command.

    Without a command, an action predicate has no state probe and stays ungraded. With one, the evaluator
    performs the probe. An answer predicate is scored here from the pinned predicate, so a failure names the
    exact expectation rather than a judge's paraphrase."""
    external = None if command is None else LocalCommandGrader(command, digest=digest, code=code, timeout=timeout)

    async def grade(ref: TaskRef, outcome: Outcome) -> Grade:
        raw = _predicate(ref)
        if raw is not None and raw.get("type") == "answer":
            return grade_answer_predicate(ref, outcome)
        if external is None:
            raise UnsupportedPredicate(f"{ref.id}: action predicate needs a trusted local state probe")
        return await external(ref, outcome)

    return grade
