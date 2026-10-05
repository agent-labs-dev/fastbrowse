"""Cross-arm comparison on one pinned external corpus, with a pinned reset before every physical attempt.

The corpus CLI runs a single arm. This driver runs more than one arm on one seeded selection, so every arm meets the
same tasks and the same independent grader: it calls the live arm runners in `fastbrowse.evals.live` rather than
copying their policies, resets the capsule immediately before each physical attempt, and keeps completion and
correctness apart. `correct` is the grader's verdict alone, while `completed` is the arm's own ending mapped
through that arm's semantics (`fastbrowse.evals.status.normalize`, `hosted=True` for the hosted agent), so no
competitor is held to fastbrowse's status vocabulary. An attempt the harness could not grade stays ungraded,
which is not a failure, and no repeat is selected as the best.

Reset is a command the caller reviewed and pinned by the digest of its code. It runs before every attempt,
retries included, through the same trusted-command seam the grader uses (no shell, an allowlisted environment,
a bounded timeout and a killed process group). A reset that fails is recorded as its own ungraded row and the
arm is never started from a capsule that was not reset.

A cloud arm cannot reach a capsule on localhost, so a hosted arm requires public endpoint URLs (a temporary
tunnel) and one URL map is used for every arm in the run. The endpoints are checked, and so are the reset and
grader pins, before the first token is spent.
"""

import argparse
import asyncio
import ipaddress
import json
import math
import os
import re
import shlex
import sys
import time
from collections.abc import Callable, Sequence
from pathlib import Path
from string import Formatter
from typing import Literal
from urllib.parse import urlparse

import httpx
from pydantic import BaseModel, ConfigDict, Field, JsonValue, TypeAdapter, ValidationError, model_validator

from fastbrowse.clients.validation import TRANSIENT_TRANSPORT
from fastbrowse.evals import corpus as corpus_module
from fastbrowse.evals import external_grade
from fastbrowse.evals.corpus import Attempt, Corpus, Grader, TaskRef, _build_grader, _source, preflight
from fastbrowse.evals.datasets import SOURCES, load_tasks
from fastbrowse.evals.external_grade import DEFAULT_TIMEOUT
from fastbrowse.evals.live import ARMS, NAVIGATION_FAILED, STUCK_SECONDS, ArmReport, ArmSpec, _down
from fastbrowse.evals.live_tasks import Category, LiveTask, Outcome
from fastbrowse.evals.status import Ending, normalize
from fastbrowse.evals.versions import provenance
from fastbrowse.models import Status, Unavailable

SCHEMA_VERSION = 1

DEFAULT_REPEATS = 1
DEFAULT_RETRIES = 0

MIN_HEADLINE_REPEATS = 3
"""A headline needs at least this many distinct repeated passes over the paired set: one pass cannot tell an arm's
level from a site's mood, and a claim over fewer repeats is refused rather than stated."""

SITE_ID = re.compile(r"^[a-z0-9][a-z0-9-]*$")
RUN_ID = re.compile(r"^[a-zA-Z0-9][a-zA-Z0-9_.-]*$")
RESET_FIELDS = frozenset({"site", "run_id"})
RESET_PLACEHOLDERS = frozenset({"{site}", "{run_id}"})
"""The only command tokens whose value is derived from the run, so the only ones safe to retain in a report."""
SECRET_NAME = re.compile(r"TOKEN|SECRET|PASSWORD|PASSWD|API[_-]?KEY|CREDENTIAL|_KEY$", re.IGNORECASE)
"""A reset or grader environment variable whose name matches this is refused: the value would be a credential
and no credential ever reaches the reset or grader process, nor the provenance written beside it."""

_NUMERISH = re.compile(r"^(?:0[xX][0-9a-fA-F]+|0[0-7]+|[0-9]+)$")
"""An IPv4 part a browser accepts but a public hostname cannot be. `127.1` and `0x7f.0.0.1` are loopback to a
browser while `ipaddress` refuses both, so a host whose parts are all numeric and not a standard dotted quad is
unroutable from the cloud and must not pass a hosted preflight."""

NO_ANSWER_ARMS = frozenset({"jev-ultrafast"})
"""Arms whose runner returns no answer. An answer predicate cannot judge them, so they are not run on answer
tasks at all rather than being scored against an answer the runner never produced."""

NO_CREDENTIAL_ARMS = frozenset({"jev-ultrafast"})
"""Arms whose runner cannot resolve `LiveTask.secrets`. A declared fixture sign-in reaches the runners that
resolve named secrets as the values themselves; this runner is handed none, so it cannot cross the login and
would be scored as a failure it could never avoid. It is not run on a declared-auth task at all."""

NO_ANSWER_REASON = "the runner returns no answer, so an answer predicate cannot judge it"
NO_CREDENTIAL_REASON = "the runner cannot resolve declared fixture credentials, so it cannot cross the sign-in"

_JSON = TypeAdapter(JsonValue)

_FIXTURE_AUTH = re.compile(r"\bas (?P<username>\S+) with password (?P<password>[^,\s]+)(?=[,\s])")
"""The one shape WindTunnel's declared-auth prompts publish fixture credentials in: `as <user> with password
<pass>`, the password ending at a comma or a space. The twelve pinned upstream prompts all use it, so a
declared-auth task whose pinned bytes do not read this way is refused at setup rather than handed to an agent as
a login it was never given the credentials to cross. These values are public fixtures from the pinned task bytes,
not a provider key or a private credential."""


class _Model(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


def jsonable(value: object) -> JsonValue:
    """A value an artifact can hold. An outcome the JSON model cannot represent becomes null, which is visible."""
    try:
        return _JSON.validate_python(value)
    except ValidationError:
        return None


def is_local_host(host: str) -> bool:
    """Whether a hostname is an address no cloud browser can route to.

    A private range, a loopback, a link-local or a carrier-grade NAT address is unroutable, and so is a
    single-label name, a `.local`, `.internal` or `.lan` name, and a wildcard-DNS name whose leading labels spell
    an IP literal (`127.0.0.1.nip.io`). A hosted arm that passed preflight on one of these would then fail from
    the cloud and be scored as an agent error.
    """
    name = host.strip("[]").lower().rstrip(".")
    if name == "localhost" or name.endswith(".localhost"):
        return True
    try:
        return not ipaddress.ip_address(name).is_global
    except ValueError:
        pass
    labels = name.split(".")
    if len(labels) == 1:
        return True
    if labels[-1] in {"local", "internal", "lan"}:
        return True
    if len(labels) < 4 and all(_NUMERISH.fullmatch(label) for label in labels):
        # `127.1`, `10.1` and `192.168.1` are private to a browser but `ipaddress` rejects them, so a shortened
        # numeric host is refused rather than handed to a cloud browser that would resolve it locally.
        return True
    if len(labels) == 4 and any(_NUMERISH.fullmatch(label) and not label.isdigit() for label in labels):
        # `0x7f.0.0.1` is the same bypass in full dotted form; a real hostname has no numeric-only parts.
        return True
    for end in range(len(labels), 3, -1):
        candidate = ".".join(labels[:end])
        try:
            address = ipaddress.ip_address(candidate)
        except ValueError:
            continue
        return not address.is_global
    return False


def validate_site_urls(sites: dict[str, str], *, remote: bool) -> dict[str, str]:
    """Check one URL map before it is pinned into the corpus.

    A start address must be a bare origin: no credentials, query or fragment may ride into the corpus a model
    sees. With a hosted arm the address must be routable from the cloud, which localhost and private ranges are
    not, so a tunnel is the only honest way to run the hosted arm against a local capsule.
    """
    for site, url in sites.items():
        parsed = urlparse(url)
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            raise ValueError(f"{site}: expected an http(s) URL, got {url!r}")
        if parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError(f"{site}: the start address must be a bare origin with no credentials or query: {url}")
        if remote and is_local_host(parsed.hostname):
            raise ValueError(
                f"{site}: a hosted arm needs a public tunnel endpoint a cloud browser can reach, not {url}"
            )
    return dict(sites)


class ResetSpec(_Model):
    """A reset command the caller reviewed: the template, the file whose digest pins it and its environment.

    The pinned code must be the program the command runs, so a reviewed script cannot be replaced by a different
    one that the digest does not cover. `{site}` and `{run_id}` are substituted per attempt; no shell sees them.
    """

    command: tuple[str, ...]
    code: Path
    digest: str = Field(pattern=r"^[a-f0-9]{64}$")
    environment: dict[str, str] = {}
    timeout: float = DEFAULT_TIMEOUT

    @model_validator(mode="after")
    def _reviewed(self) -> "ResetSpec":
        if not self.command:
            raise ValueError("reset command must not be empty")
        if str(self.code) not in self.command:
            raise ValueError("reset code must name the program the pinned command runs")
        if not math.isfinite(self.timeout) or self.timeout <= 0:
            raise ValueError("reset timeout must be a positive number of seconds")
        for part in self.command:
            try:
                fields = {field for _, field, _, _ in Formatter().parse(part) if field is not None}
            except ValueError as exc:
                raise ValueError(f"reset command has a malformed placeholder in {part!r}: {exc}") from exc
            if unknown := fields - RESET_FIELDS:
                raise ValueError(f"reset command has an unknown placeholder: {', '.join(sorted(unknown))}")
        return self

    def uses(self, field: str) -> bool:
        return any(field in {name for _, name, _, _ in Formatter().parse(part) if name} for part in self.command)

    def resolved(self, *, site: str, run_id: str) -> tuple[str, ...]:
        """The command for one attempt, after checking the substituted values cannot smuggle an argument."""
        if RUN_ID.fullmatch(run_id) is None:
            raise ValueError(f"invalid run id: {run_id!r}")
        if self.uses("site") and SITE_ID.fullmatch(site) is None:
            raise ValueError(f"invalid capsule site id: {site!r}")
        return tuple(part.format(site=site, run_id=run_id) for part in self.command)


class ResetRecord(_Model):
    """What one reset did, with the environment names and never a value, so no credential lands in a ledger."""

    command: tuple[str, ...]
    code: str
    sha256: str
    environment: tuple[str, ...]
    ok: bool
    seconds: float
    error: str | None = None


def _redacted_command(command: Sequence[str]) -> list[str]:
    """The reset command for an artifact: the program and the run placeholders, never a literal argument.

    The command line is where a caller's own credential would sit (a flag such as `--token abc`), so every other
    token is dropped before the command is pinned into provenance or a reset record.
    """
    return [command[0], *(part for part in command[1:] if part in RESET_PLACEHOLDERS)] if command else []


def _reset_secrets(spec: "ResetSpec") -> tuple[str, ...]:
    """Every literal a reset could echo: its non-placeholder arguments and all declared environment values."""
    return tuple(
        value for value in (*spec.command[1:], *spec.environment.values()) if value and value not in RESET_PLACEHOLDERS
    )


def _redact(text: str, secrets: Sequence[str]) -> str:
    for secret in secrets:
        if secret:
            text = text.replace(secret, "[redacted]")
    return text


def reset_environment(spec: ResetSpec, source: dict[str, str] | None = None) -> dict[str, str]:
    """The reset process's environment: the grader's allowlist plus the caller's declared names.

    A name that reads like a credential is refused rather than passed, because a provider key in the reset
    environment is a secret a command we did not write would receive.
    """
    for name in spec.environment:
        if SECRET_NAME.search(name):
            raise ValueError(f"reset environment variable looks like a credential and is refused: {name}")
    return external_grade.safe_environment(source) | dict(spec.environment)


async def run_reset(spec: ResetSpec, *, site: str, run_id: str, payload: bytes) -> ResetRecord:
    """Reset the capsule once, through the trusted-command seam, and never raise: the outcome is a record.

    The code is re-read before every run, as the grader's is, so a script edited after it was pinned cannot keep
    running under the old digest. A bad substitution, a missing code file or a credential name becomes a failed
    record rather than an exception that would abandon the run with money already spent. Cancellation propagates so
    an interrupted run stops at once. The command is retained redacted, and reset output and errors have every
    literal the reset could echo replaced, so no credential reaches a ledger.
    """
    started = time.monotonic()
    command = _redacted_command(spec.command)
    secrets = _reset_secrets(spec)
    names: tuple[str, ...] = ()

    def record(*, ok: bool, error: str | None = None) -> ResetRecord:
        return ResetRecord(
            command=tuple(command),
            code=str(spec.code),
            sha256=spec.digest,
            environment=names,
            ok=ok,
            seconds=round(time.monotonic() - started, 2),
            error=None if error is None else _redact(error, secrets),
        )

    try:
        environment = reset_environment(spec)
        names = tuple(sorted(environment))
        current = external_grade.file_digest(spec.code)
        if current != spec.digest:
            return record(ok=False, error=f"reset code changed since it was pinned: {current} != {spec.digest}")
        resolved = spec.resolved(site=site, run_id=run_id)
        # The same no-shell, process-group-killing seam the grader uses; stdout is dropped, not retained.
        await external_grade._run_command(resolved, payload, seconds=spec.timeout, environment=environment)
    except external_grade.GraderError as exc:
        return record(ok=False, error=str(exc))
    except (ValueError, OSError) as exc:
        return record(ok=False, error=f"{type(exc).__name__}: {exc}")
    return record(ok=True)


class ArmAttempt(_Model):
    """One physical attempt by one arm, with the reset that preceded it and the canonical attempt it produced.

    `correct` is the grader's verdict alone; `completed` stays on the nested `Attempt`, which also keeps the
    stricter `passed` (grader pass and completion). Keeping both is what lets a correct answer to a task the arm
    never called done be told apart from a wrong one.
    """

    arm: str
    pin: str
    tier: Literal["A", "hosted"]
    model: str | None = None
    retry: int = 0
    reset: ResetRecord
    attempt: Attempt

    @property
    def correct(self) -> bool | None:
        """The grade alone, or None while ungraded. Never folded together with the arm's own completion."""
        return None if self.attempt.grade is None else self.attempt.grade.passed


def completion_for(spec: ArmSpec, report: ArmReport) -> Ending:
    """The arm's own ending, in its own vocabulary. The hosted agent's `stopped` or `idle` with an answer is a
    completion, where that same status on fastbrowse is not."""
    return normalize(report.status, hosted=spec.tier == "hosted", answered=bool(report.answered))


def arm_eligible(arm: str, ref: TaskRef) -> str | None:
    """Why an arm cannot be judged on a task, or None when it can. Two closures keep a pair from being dropped for
    the wrong reason: an answer predicate needs an answer the runner never returns, and a declared fixture sign-in is
    supplied as named secrets a runner that does not resolve them cannot be given. A report says which one applied."""
    raw = ref.metadata.get("predicate")
    if arm in NO_ANSWER_ARMS and isinstance(raw, dict) and raw.get("type") == "answer":
        return NO_ANSWER_REASON
    if arm in NO_CREDENTIAL_ARMS and _fixture_match(ref) is not None:
        return NO_CREDENTIAL_REASON
    return None


async def _unused_truth(_: httpx.AsyncClient) -> object:
    raise AssertionError("the corpus grader is the answer key, not live task truth")


def _unused_check(_: Outcome, __: object) -> str | None:
    raise AssertionError("the corpus grader is the answer key, not live task truth")


def _fixture_match(ref: TaskRef) -> re.Match[str] | None:
    """The credential spans a declared-auth task holds, or None when the task declares no sign-in.

    The task bytes are pinned upstream text, so the credentials are found by one exact shape and a declared-auth
    task that does not have exactly one is an error: guessing at a different format would either send a model the
    real secret or let an arm fail a login the setup should have refused before a token was spent.
    """
    if ref.source != "windtunnel" or ref.metadata.get("auth") is not True:
        return None
    matches = list(_FIXTURE_AUTH.finditer(ref.task))
    if len(matches) != 1:
        raise ValueError(
            f"{ref.id}: declares fixture auth but its pinned prompt has {len(matches)} "
            "'as <user> with password <pass>' spans, expected exactly one"
        )
    return matches[0]


def _fixture_placeholders(task: str, match: re.Match[str]) -> str:
    """`task` with only the matched username and password spans replaced by the secret names that resolve them."""
    return (
        task[: match.start("username")]
        + "username"
        + task[match.end("username") : match.start("password")]
        + "password"
        + task[match.end("password") :]
    )


def fixture_credentials(refs: Sequence[TaskRef]) -> dict[str, list[str]]:
    """The credential names each declared-auth task resolves, for the run protocol.

    Only the names are recorded, never the public fixture values: the protocol says which secrets a task needed,
    not what they were. Parsing every declared-auth task here is also what refuses an unexpected prompt during
    setup, before any arm is prepared or spends.
    """
    named: dict[str, list[str]] = {}
    for ref in refs:
        if _fixture_match(ref) is not None:
            named[ref.id] = ["password", "username"]
    return named


def live_task(ref: TaskRef, *, authorize: bool) -> LiveTask:
    """The live arm runners take a `LiveTask`; this gives them the pinned corpus fields and a fixed category so
    every arm and task runs under the same browser policy. Truth and check are unused, because grading is the
    corpus grader's, and they raise if a runner ever reaches for them.

    A declared fixture sign-in is replaced by its secret names in the text a model sees and supplied as `secrets`
    scoped to the start origin, so the fastbrowse arm's resolver and the hosted arm's named sensitive data hold the
    same public fixture values without either model ever reading them. The `TaskRef` and its digest are untouched,
    so the corpus grader still judges the original task."""
    match = _fixture_match(ref)
    secrets = {} if match is None else {"username": match["username"], "password": match["password"]}
    return LiveTask(
        id=ref.id,
        start=ref.start,
        task=ref.task if match is None else _fixture_placeholders(ref.task, match),
        truth=_unused_truth,
        check=_unused_check,
        category=Category.LOOKUP,
        secrets=secrets,
        authorize=authorize,
    )


def _write_artifact(downloads: Path, payload: dict[str, object]) -> Path:
    """Write one attempt's artifact exclusively and durably: a second write to the same path is an error, not a
    silent overwrite, and the bytes are on disk before the ledger row that names it."""
    downloads.mkdir(parents=True, exist_ok=True)
    return _write_text(downloads / "arm-report.json", json.dumps(payload, indent=2, default=str) + "\n")


async def run_arm_attempt(
    ref: TaskRef,
    *,
    corpus: Corpus,
    repeat: int,
    retry: int,
    spec: ArmSpec,
    arm: str,
    http: httpx.AsyncClient,
    downloads: Path,
    run: dict[str, JsonValue],
    grader: Grader,
    reset: ResetSpec,
    run_id: str,
    authorize: bool,
    on_attempt: Callable[[ArmAttempt], None] | None = None,
) -> ArmAttempt:
    """Reset, then one arm attempt, then one grade, with every ending recorded.

    A reset failure, an arm that raises and a grader that raises each leave the attempt ungraded and name why;
    only an arm that returned an outcome is graded. Completion is the arm's own, so a hosted arm that answered
    is complete even though its status is not fastbrowse's. A runner that outlives the live suite's stuck bound is
    stopped as an outage, and an interrupt during the arm or the grade writes its own unknown-cost row before the
    cancellation is re-raised, so no in-flight spend is dropped.
    """
    await asyncio.to_thread(downloads.mkdir, parents=True, exist_ok=True)
    payload = json.dumps({"site": ref.site, "run_id": run_id, "task": ref.id, "repeat": repeat, "retry": retry})
    reset_record = await run_reset(reset, site=ref.site or "", run_id=run_id, payload=payload.encode("utf-8"))
    started = time.monotonic()
    outcome: Outcome | None = None
    report: ArmReport | None = None
    error: str | None = None
    grade = None
    grade_error: str | None = None
    raw_status: str | None = None
    ending = Ending.ERROR
    interrupted = False
    cancelled: BaseException | None = None
    if not reset_record.ok:
        # The capsule was not reset, so the arm is not started: a stale state would measure the previous attempt.
        error = f"reset failed: {reset_record.error}"
        grade_error = error
    else:
        task = live_task(ref, authorize=authorize)
        try:
            async with asyncio.timeout(STUCK_SECONDS) as cap:
                outcome, report = await spec.runner(
                    task, http, downloads, bitwarden=False, record=None, started=started
                )
        except (asyncio.CancelledError, KeyboardInterrupt) as exc:  # the row is written below, then re-raised
            cancelled = exc
            interrupted = True
            error = grade_error = "interrupted"
        except Exception as exc:  # one arm crashing is that attempt's row, never the end of the comparison
            timed_out = cap.expired()
            cause = Unavailable(f"still running after {STUCK_SECONDS // 60} minutes") if timed_out else exc
            unavailable = isinstance(cause, (Unavailable, *TRANSIENT_TRANSPORT))
            error = f"{type(cause).__name__}: {cause}"
            raw_status = Status.UNAVAILABLE.value if unavailable else None
            ending = Ending.UNAVAILABLE if unavailable else Ending.ERROR
            grade_error = error
        else:
            raw_status = report.status
            ending = completion_for(spec, report)
            if report.failure_class is not None and ending is not Ending.DONE:
                # The arm reported its own browser or the site failing: an outage measures no arm, so it is left
                # ungraded and a retry may wait it out.
                ending = Ending.UNAVAILABLE
            if ending is Ending.UNAVAILABLE:
                grade_error = f"outage: {report.status} ({report.failure_class or 'provider unavailable'})"
            else:
                try:
                    grade = await grader(ref, outcome)
                except (asyncio.CancelledError, KeyboardInterrupt) as exc:
                    cancelled = exc
                    interrupted = True
                    error = grade_error = "interrupted"
                except Exception as exc:
                    grade_error = f"grader raised {type(exc).__name__}: {exc}"
        if not interrupted and grade is None and ending is not Ending.UNAVAILABLE:
            try:
                down = await _down(task, http)
            except (asyncio.CancelledError, KeyboardInterrupt) as exc:
                cancelled = exc
                interrupted = True
                error = grade_error = "interrupted"
            else:
                if down is not None:
                    # The harness's own look at the site, as the live suite does: a site down measures no arm.
                    ending = Ending.UNAVAILABLE
                    grade_error = down
    if (
        not interrupted
        and ending is Ending.UNAVAILABLE
        and NAVIGATION_FAILED in (error or "")
        and await _down(task, http) is None
    ):
        # A first page that never loaded is an outage only when the site is down; a site that answers means this
        # arm's browser failed, which is that attempt's error and not a provider outage to retry.
        ending = Ending.ERROR
        raw_status = None
        grade_error = error
    seconds = round(time.monotonic() - started, 2)
    if interrupted:
        # The attempt may have spent before it was stopped, and an interrupted grade cannot price it.
        dollars: float | None = None
        unknown_cost = True
        ending = Ending.ERROR
        raw_status = None
        grade = None
        grade_error = "interrupted"
    elif not reset_record.ok:
        # Nothing ran, so the spend is known to be nothing rather than unknown.
        dollars = 0.0
        unknown_cost = False
    elif report is None:
        # A crashed attempt may have spent before it failed, and that spend cannot be priced from no report.
        dollars = None
        unknown_cost = True
    else:
        dollars = report.dollars
        # A None price is itself the unknown: the known total would only be a floor, so it is not reported as one.
        unknown_cost = report.dollars is None or bool(report.unknown_cost)
    model = None if report is None else (report.model or report.text_model)
    artifact = _write_artifact(
        downloads,
        {
            "arm": arm,
            "pin": spec.pin,
            "tier": spec.tier,
            "model": model,
            "retry": retry,
            "reset": reset_record.model_dump(mode="json"),
            "error": error,
            "outcome": None
            if outcome is None
            else {
                "answer": outcome.answer,
                "final_url": outcome.final_url,
                "unobservable": outcome.unobservable,
            },
            "report": None if report is None else report.model_dump(),
        },
    )
    attempt = Attempt(
        corpus=corpus.digest,
        source=ref.source,
        revision=ref.revision,
        sha256=ref.sha256,
        task=ref.id,
        task_digest=ref.digest,
        stratum=ref.stratum,
        repeat=repeat,
        arm=arm,
        raw_status=raw_status,
        completion=ending,
        completed=ending is Ending.DONE,
        graded=grade is not None,
        grade=grade,
        passed=None if grade is None else grade.passed and ending is Ending.DONE,
        grade_error=grade_error,
        seconds=seconds,
        dollars=None if dollars is None else round(dollars, 5),
        unknown_cost=unknown_cost,
        answer=None if outcome is None else outcome.answer,
        data=None if outcome is None else jsonable(outcome.data),
        final_url=None if outcome is None else outcome.final_url,
        error=error,
        run_artifact=str(artifact),
        run=dict(run),
    )
    row = ArmAttempt(
        arm=arm, pin=spec.pin, tier=spec.tier, model=model, retry=retry, reset=reset_record, attempt=attempt
    )
    if cancelled is not None:
        # The ledger row is durable before the interrupt propagates, so an in-flight attempt is never lost.
        if on_attempt is not None:
            on_attempt(row)
        raise cancelled
    return row


class SkipRecord(_Model):
    arm: str
    task: str
    task_digest: str
    reason: str


class ComparisonResult(_Model):
    attempts: tuple[ArmAttempt, ...]
    skipped: tuple[SkipRecord, ...]
    truncated: bool


def _rotate(arms: Sequence[str], step: int) -> tuple[str, ...]:
    """The arm order for one repeat. Rotating by the repeat index keeps the first run of a fresh browser or a
    cold site from always falling to the same arm. Arms run task by task, so the same task's arms stay adjacent
    in time and a site that degrades mid-run cannot fall on one arm alone."""
    if not arms:
        return ()
    offset = step % len(arms)
    return tuple(arms[offset:]) + tuple(arms[:offset])


async def run_comparison(
    corpus: Corpus,
    *,
    specs: dict[str, ArmSpec],
    arms: tuple[str, ...],
    http: httpx.AsyncClient,
    downloads: Path,
    run: dict[str, JsonValue],
    grader: Grader,
    reset: ResetSpec,
    run_id: str,
    repeats: int = DEFAULT_REPEATS,
    retries: int = DEFAULT_RETRIES,
    authorize: bool = False,
    max_attempts: int | None = None,
    on_attempt: Callable[[ArmAttempt], None] | None = None,
    on_skip: Callable[[SkipRecord], None] | None = None,
) -> ComparisonResult:
    """Every eligible task, arm and repeat, with the arm order rotated each repeat.

    Retries exist for a provider outage only, and every retry resets first and is kept as its own physical row.
    A task failure is never retried, so no repeat is quietly promoted to the best one. `on_skip` is called as each
    pair is skipped, so a caller that is cancelled mid-run still holds the skips that had been decided.
    """
    if repeats < 1:
        raise ValueError("repeats must be positive")
    if retries < 0:
        raise ValueError("retries must not be negative")
    plan = [(repeat, ref, arm) for repeat in range(repeats) for ref in corpus.tasks for arm in _rotate(arms, repeat)]
    attempts: list[ArmAttempt] = []
    skipped: list[SkipRecord] = []
    truncated = False
    for repeat, ref, arm in plan:
        reason = arm_eligible(arm, ref)
        if reason is not None:
            skip = SkipRecord(arm=arm, task=ref.id, task_digest=ref.digest, reason=reason)
            skipped.append(skip)
            if on_skip is not None:
                on_skip(skip)
            continue
        for retry in range(retries + 1):
            if max_attempts is not None and len(attempts) >= max_attempts:
                truncated = True
                break
            attempt = await run_arm_attempt(
                ref,
                corpus=corpus,
                repeat=repeat,
                retry=retry,
                spec=specs[arm],
                arm=arm,
                http=http,
                downloads=downloads / arm / ref.digest[:16] / f"repeat-{repeat}" / f"retry-{retry}",
                run=run,
                grader=grader,
                reset=reset,
                run_id=run_id,
                authorize=authorize,
                on_attempt=on_attempt,
            )
            attempts.append(attempt)
            if on_attempt is not None:
                on_attempt(attempt)
            if attempt.attempt.completion is not Ending.UNAVAILABLE:
                break
        if truncated:
            break
    return ComparisonResult(
        attempts=tuple(attempts),
        skipped=tuple(skipped),
        truncated=truncated,
    )


class ArmTotals(_Model):
    attempts: int
    completed: int
    graded: int
    correct: int
    ungraded: int
    unavailable: int
    unknown_cost: int
    dollars: float | None
    """None when any attempt's spend could not be priced: a known total would then be only a floor."""
    known_dollars: float


class PairedArm(_Model):
    graded: int
    correct: int
    completed: int


class AttemptRecord(_Model):
    """One line of the report: the physical attempt named by task and arm, completion and correctness apart."""

    arm: str
    model: str | None = None
    task: str
    task_digest: str
    stratum: str
    repeat: int
    retry: int
    reset_ok: bool
    completion: Ending
    completed: bool
    graded: bool
    correct: bool | None
    unavailable: bool
    seconds: float
    dollars: float | None
    unknown_cost: bool
    error: str | None = None
    grade_error: str | None = None


class ComparisonReport(_Model):
    schema_version: Literal[1] = 1
    corpus: str
    source: str
    revision: str
    sha256: str
    seed: int
    per_stratum: int
    task_count: int
    site_urls: dict[str, str]
    remote: bool
    setup_seconds: float | None
    preflight_seconds: float
    prepare_seconds: dict[str, float]
    grader: dict[str, JsonValue]
    reset: dict[str, JsonValue]
    arms: dict[str, dict[str, JsonValue]]
    attempts: tuple[AttemptRecord, ...]
    totals: dict[str, ArmTotals]
    skipped: tuple[SkipRecord, ...]
    repeats: int
    paired_tasks: tuple[tuple[str, int], ...]
    paired: dict[str, PairedArm]
    truncated: bool
    interrupted: bool
    headline: str
    note: str


def arm_totals(attempts: Sequence[ArmAttempt], arm: str) -> ArmTotals:
    mine = [item for item in attempts if item.arm == arm]
    priced = [item.attempt.dollars for item in mine if item.attempt.dollars is not None]
    unknown = sum(item.attempt.unknown_cost for item in mine)
    return ArmTotals(
        attempts=len(mine),
        completed=sum(item.attempt.completed for item in mine),
        graded=sum(item.attempt.graded for item in mine),
        correct=sum(item.correct is True for item in mine),
        ungraded=sum(not item.attempt.graded for item in mine),
        unavailable=sum(item.attempt.completion is Ending.UNAVAILABLE for item in mine),
        unknown_cost=unknown,
        dollars=None if unknown else round(sum(priced), 5),
        known_dollars=round(sum(priced), 5),
    )


def paired_tasks(
    corpus: Corpus, arms: Sequence[str], attempts: Sequence[ArmAttempt], repeats: int
) -> tuple[tuple[str, int], ...]:
    """The `(task digest, repeat)` units every requested arm graded, so no arm is compared on a different denominator.

    A task where any requested arm is ineligible, ungraded or missing at a repeat is left out entirely: an arm-to-arm
    figure over different task sets is not a comparison. Scattered repeats across tasks cannot supply a task's
    three-repeat floor; excluded attempts remain in the report."""
    shared: list[tuple[str, int]] = []
    for ref in corpus.tasks:
        graded_repeats = [
            repeat
            for repeat in range(repeats)
            if all(
                any(
                    item.attempt.graded
                    and item.arm == arm
                    and item.attempt.task_digest == ref.digest
                    and item.attempt.repeat == repeat
                    for item in attempts
                )
                for arm in arms
            )
        ]
        if len(graded_repeats) >= MIN_HEADLINE_REPEATS:
            shared.extend((ref.digest, repeat) for repeat in graded_repeats)
    return tuple(shared)


def paired_totals(
    attempts: Sequence[ArmAttempt], arms: Sequence[str], shared: Sequence[tuple[str, int]]
) -> dict[str, PairedArm]:
    keyed = set(shared)
    totals: dict[str, PairedArm] = {}
    for arm in arms:
        mine = [
            item for item in attempts if item.arm == arm and (item.attempt.task_digest, item.attempt.repeat) in keyed
        ]
        totals[arm] = PairedArm(
            graded=sum(item.attempt.graded for item in mine),
            correct=sum(item.correct is True for item in mine),
            completed=sum(item.attempt.completed for item in mine),
        )
    return totals


def headline_for(arms: Sequence[str], shared: Sequence[tuple[str, int]], paired: dict[str, PairedArm]) -> str:
    """A comparison sentence over the per-task adequate pairs, or an explicit refusal to state one."""
    if not shared:
        return (
            "INSUFFICIENT PAIRED DATA: no task has at least "
            f"{MIN_HEADLINE_REPEATS} repeated passes graded by every requested arm, so no "
            "arm-to-arm comparison is reported."
        )
    tasks = len({digest for digest, _ in shared})
    parts = [
        f"{arm} correct {paired[arm].correct}/{paired[arm].graded} (completed {paired[arm].completed})" for arm in arms
    ]
    return f"paired on {len(shared)} task-repeats across {tasks} tasks: " + "; ".join(parts)


def build_report(
    corpus: Corpus,
    *,
    arms: tuple[str, ...],
    attempts: Sequence[ArmAttempt],
    skipped: Sequence[SkipRecord],
    truncated: bool,
    interrupted: bool = False,
    repeats: int = DEFAULT_REPEATS,
    sites: dict[str, str],
    remote: bool,
    setup_seconds: float | None,
    preflight_seconds: float,
    prepare_seconds: dict[str, float],
    grader: dict[str, JsonValue],
    reset: dict[str, JsonValue],
    specs: dict[str, ArmSpec],
) -> ComparisonReport:
    shared = paired_tasks(corpus, arms, attempts, repeats)
    paired = paired_totals(attempts, arms, shared)
    totals = {arm: arm_totals(attempts, arm) for arm in arms}
    records = tuple(
        AttemptRecord(
            arm=item.arm,
            model=item.model,
            task=item.attempt.task,
            task_digest=item.attempt.task_digest,
            stratum=item.attempt.stratum,
            repeat=item.attempt.repeat,
            retry=item.retry,
            reset_ok=item.reset.ok,
            completion=item.attempt.completion,
            completed=item.attempt.completed,
            graded=item.attempt.graded,
            correct=item.correct,
            unavailable=item.attempt.completion is Ending.UNAVAILABLE,
            seconds=item.attempt.seconds,
            dollars=item.attempt.dollars,
            unknown_cost=item.attempt.unknown_cost,
            error=item.attempt.error,
            grade_error=item.attempt.grade_error,
        )
        for item in attempts
    )
    note = (
        "Completion and correctness are counted apart; an ungraded attempt has no correctness state, and a "
        "provider outage is kept as its own row and never scored."
    )
    if truncated:
        note += " The attempt cap stopped the run before every planned attempt ran."
    if interrupted:
        note += " An interrupt stopped the run; only the attempts already written are reported."
    return ComparisonReport(
        corpus=corpus.digest,
        source=corpus.source,
        revision=corpus.revision,
        sha256=corpus.sha256,
        seed=corpus.seed,
        per_stratum=corpus.per_stratum,
        task_count=len(corpus.tasks),
        site_urls=dict(sites),
        remote=remote,
        setup_seconds=setup_seconds,
        preflight_seconds=round(preflight_seconds, 2),
        prepare_seconds=dict(prepare_seconds),
        grader=dict(grader),
        reset=dict(reset),
        arms={arm: {"pin": specs[arm].pin, "tier": specs[arm].tier} for arm in arms},
        attempts=records,
        totals=totals,
        skipped=tuple(skipped),
        repeats=repeats,
        paired_tasks=shared,
        paired=paired,
        truncated=truncated,
        interrupted=interrupted,
        headline=(
            "INSUFFICIENT PAIRED DATA: the draw stopped before completion, so no arm-to-arm comparison is reported."
            if truncated or interrupted
            else headline_for(arms, shared, paired)
        ),
        note=note,
    )


def _run_cleanly(attempts: Sequence[ArmAttempt]) -> bool:
    return bool(attempts) and all(
        item.reset.ok and item.attempt.graded and item.attempt.completion is not Ending.UNAVAILABLE for item in attempts
    )


def _load_sites(path: Path | None) -> dict[str, str] | None:
    if path is None:
        return None
    body = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(body, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in body.items()):
        raise ValueError(f"{path}: expected a JSON object of site id to URL")
    return dict(body)


def _build_reset(args: argparse.Namespace) -> ResetSpec | None:
    if args.reset_command is None:
        if args.reset_code or args.reset_sha256 or args.reset_env or args.reset_timeout:
            raise ValueError("--reset-code, --reset-sha256, --reset-env and --reset-timeout need --reset-command")
        return None
    if args.reset_code is None or args.reset_sha256 is None:
        raise ValueError("--reset-command requires --reset-code and --reset-sha256 to pin the reset program")
    command = tuple(shlex.split(args.reset_command))
    if not command:
        raise ValueError("--reset-command parsed to no tokens")
    code = args.reset_code.resolve()
    if not code.is_file():
        raise ValueError(f"reset code is not a file: {code}")
    if command[0] != str(code):
        raise ValueError("the pinned reset code must be the program the reset command runs")
    environment = {} if args.reset_env is None else json.loads(args.reset_env.read_text(encoding="utf-8"))
    if not isinstance(environment, dict) or not all(
        isinstance(k, str) and isinstance(v, str) for k, v in environment.items()
    ):
        raise ValueError(f"{args.reset_env}: expected a JSON object of name to value")
    spec = ResetSpec(
        command=command,
        code=code,
        digest=args.reset_sha256,
        environment=dict(environment),
        timeout=args.reset_timeout if args.reset_timeout is not None else DEFAULT_TIMEOUT,
    )
    if external_grade.file_digest(code) != spec.digest:
        raise ValueError("reset code digest does not match the pinned digest")
    reset_environment(spec)  # refuse a credential name before the run, not at the first attempt
    return spec


def _is_probe(ref: TaskRef) -> bool:
    raw = ref.metadata.get("predicate")
    return isinstance(raw, dict) and raw.get("probe") is not None


def _recorded_argv(argv: Sequence[str]) -> list[str]:
    """The canonical argv redaction, plus `--reset-command`, whose single quoted argument can carry a credential."""
    redacted: list[str] = []
    arguments = iter(argv)
    for argument in arguments:
        if argument == "--reset-command":
            redacted.extend((argument, "[redacted]"))
            next(arguments, None)
        elif argument.startswith("--reset-command="):
            redacted.append("--reset-command=[redacted]")
        else:
            redacted.append(argument)
    return corpus_module._recorded_argv(redacted)


def _write_text(path: Path, text: str) -> Path:
    """Create-and-write, so a file this run owns is never silently replaced, and fsync before returning."""
    with path.open("x", encoding="utf-8") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    return path


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("source", choices=list(SOURCES))
    parser.add_argument("--cache", type=Path)
    parser.add_argument("--sha256", help="verified digest for the pinned gated Online-Mind2Web file")
    parser.add_argument("--site-urls", type=Path, help="local capsule endpoints as a JSON object of site id to URL")
    parser.add_argument(
        "--public-site-urls",
        type=Path,
        help="tunnel endpoints as the same JSON object; required for a hosted arm, and used by every arm then",
    )
    parser.add_argument("--per-stratum", type=int, default=1)
    parser.add_argument("--seed", type=int, default=20260925)
    parser.add_argument(
        "--arms",
        nargs="*",
        default=[name for name, spec in ARMS.items() if spec.default],
        choices=list(ARMS),
    )
    parser.add_argument("--repeat", type=int, default=DEFAULT_REPEATS)
    parser.add_argument("--retries", type=int, default=DEFAULT_RETRIES)
    parser.add_argument("--max-attempts", type=int, help="hard cap on physical attempts, including retries")
    parser.add_argument("--setup-seconds", type=float, help="tunnel and capsule boot seconds the operator measured")
    parser.add_argument("--authorize", action="store_true", help="authorize irreversible actions for every arm")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--execute", action="store_true", help="run the arms; without it this is preflight only")
    parser.add_argument("--reset-command", help="pinned reset as one quoted string with {site} and {run_id}")
    parser.add_argument("--reset-code", type=Path, help="the reset program's file, whose sha256 pins the command")
    parser.add_argument("--reset-sha256", help="required sha256 of --reset-code")
    parser.add_argument("--reset-env", type=Path, help="JSON object of extra reset environment variables")
    parser.add_argument("--reset-timeout", type=float)
    parser.add_argument(
        "--grader-command",
        nargs=argparse.REMAINDER,
        metavar="TOKEN",
        help="trusted evaluator as an argument vector, given last; stdin gets one JSON request, stdout one Grade",
    )
    parser.add_argument("--grader-code", type=Path)
    parser.add_argument("--grader-sha256")
    parser.add_argument("--grader-timeout", type=float)
    return parser


async def main(argv: list[str]) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    if args.retries < 0:
        parser.error("--retries must not be negative")
    if args.max_attempts is not None and args.max_attempts < 1:
        parser.error("--max-attempts must be positive")
    if args.grader_command and args.grader_command[0] == "--":
        args.grader_command = args.grader_command[1:]
    args.arms = list(dict.fromkeys(args.arms))
    if not args.arms:
        parser.error("--arms must name at least one arm")
    try:
        reset = _build_reset(args)
        sites_local = _load_sites(args.site_urls)
        sites_public = _load_sites(args.public_site_urls)
        hosted = any(ARMS[arm].tier == "hosted" for arm in args.arms)
        if hosted and sites_public is None:
            parser.error("a hosted arm needs --public-site-urls; a cloud browser cannot reach a capsule on localhost")
        sites = sites_public if hosted else sites_local
        if args.source == "windtunnel" and sites is None:
            parser.error("windtunnel requires --site-urls (and --public-site-urls for a hosted arm)")
        sites = validate_site_urls(sites or {}, remote=hosted)
        if args.execute and reset is None:
            parser.error("--execute requires a pinned reset (--reset-command, --reset-code, --reset-sha256)")
        if args.grader_command is None and (args.grader_code or args.grader_sha256 or args.grader_timeout):
            raise ValueError("--grader-code, --grader-sha256 and --grader-timeout need --grader-command")
        grader = _build_grader(args)
        source = _source(args)
    except ValueError as exc:
        parser.error(str(exc))
    run = TypeAdapter(dict[str, JsonValue]).validate_python(
        provenance(
            source=source.id,
            argv=_recorded_argv(argv),
            arms={arm: {"pin": ARMS[arm].pin, "tier": ARMS[arm].tier} for arm in args.arms},
            sites=sites,
            remote=hosted,
            seed=args.seed,
            per_stratum=args.per_stratum,
            repeats=args.repeat,
            retries=args.retries,
        )
    )
    # Only the grader program is retained: a later argument can carry a credential, and the pin is code plus digest.
    grader_pin: dict[str, JsonValue] = {
        "command": list(args.grader_command or [])[:1],
        "code": None if args.grader_code is None else str(args.grader_code),
        "sha256": args.grader_sha256,
    }
    reset_pin: dict[str, JsonValue] = (
        {
            "command": jsonable(_redacted_command(reset.command)),
            "code": str(reset.code),
            "sha256": reset.digest,
            "timeout": reset.timeout,
        }
        if reset is not None
        else {"command": None, "code": None, "sha256": None, "timeout": None}
    )
    started = time.monotonic()
    async with httpx.AsyncClient(timeout=60) as http:
        try:
            tasks = await load_tasks(source, http, cache=args.cache, token=os.environ.get("HF_TOKEN"), site_urls=sites)
            corpus = Corpus.build(source, tasks, per_stratum=args.per_stratum, seed=args.seed)
            # Refuse an unexpected declared-auth prompt here, before any arm is prepared or a token is spent.
            declared_auth = fixture_credentials(corpus.tasks)
        except (ValueError, httpx.HTTPError) as exc:
            parser.error(str(exc))
        run["fixture_auth"] = jsonable(declared_auth)
        if args.out.exists():
            parser.error(f"refusing to overwrite existing output directory: {args.out}")
        try:
            await asyncio.to_thread(args.out.mkdir, parents=True, exist_ok=False)
        except FileExistsError:
            parser.error(f"refusing to overwrite existing output directory: {args.out}")
        _write_text(
            args.out / "corpus.json",
            json.dumps({"run": run, "corpus": corpus.model_dump(mode="json")}, indent=2) + "\n",
        )
        checks = await preflight(corpus, http)
        _write_text(args.out / "preflight.jsonl", "".join(row.model_dump_json() + "\n" for row in checks))
        preflight_seconds = time.monotonic() - started
        unreachable = [row for row in checks if not row.reachable]
        probes = [ref for ref in corpus.tasks if _is_probe(ref)]
        print(f"CORPUS {corpus.digest} {len(corpus.tasks)} tasks, {len(checks) - len(unreachable)} reachable")
        if not args.execute:
            print(f"PREFLIGHT only; wrote {args.out}. Pass --execute to run the arms.")
            return 1 if unreachable else 0
        if probes and args.grader_command is None:
            print(f"REFUSED: {len(probes)} probe tasks need --grader-command; nothing was spent", flush=True)
            return 2
        if unreachable:
            names = ", ".join(row.task for row in unreachable)
            print(f"REFUSED: unreachable start addresses ({names}); nothing was spent", flush=True)
            return 2
        specs = {arm: ARMS[arm] for arm in args.arms}
        assert reset is not None  # the execute guard above refused a run without a pinned reset
        prepare_seconds: dict[str, float] = {}
        for arm in args.arms:
            if prepare := ARMS[arm].prepare:
                began = time.monotonic()
                await prepare()
                prepare_seconds[arm] = round(time.monotonic() - began, 2)
        attempts: list[ArmAttempt] = []
        skipped_rows: list[SkipRecord] = []
        truncated = False
        interrupted = False
        try:
            with (args.out / "attempts.jsonl").open("x", encoding="utf-8") as ledger:

                def write(attempt: ArmAttempt) -> None:
                    attempts.append(attempt)
                    ledger.write(attempt.model_dump_json() + "\n")
                    ledger.flush()
                    os.fsync(ledger.fileno())
                    mark = "ungraded" if not attempt.attempt.graded else "correct" if attempt.correct else "wrong"
                    print(
                        f"{mark:9} {attempt.arm:14} {attempt.attempt.task:22} "
                        f"{attempt.attempt.completion.value} {attempt.attempt.seconds}s",
                        flush=True,
                    )

                result = await run_comparison(
                    corpus,
                    specs=specs,
                    arms=tuple(args.arms),
                    http=http,
                    downloads=args.out / "downloads",
                    run=run,
                    grader=grader,
                    reset=reset,
                    run_id=str(run["run_id"]),
                    repeats=args.repeat,
                    retries=args.retries,
                    authorize=args.authorize,
                    max_attempts=args.max_attempts,
                    on_attempt=write,
                    on_skip=skipped_rows.append,
                )
                truncated = result.truncated
        except KeyboardInterrupt:
            interrupted = True
            print("interrupted; finished attempts are on disk", flush=True)
        except asyncio.CancelledError:
            interrupted = True
            print("cancelled; finished attempts are on disk", flush=True)
            raise
        finally:
            # Runs on interruption too, so the report counts every finished attempt and never claims the rest.
            report = build_report(
                corpus,
                arms=tuple(args.arms),
                attempts=attempts,
                skipped=tuple(skipped_rows),
                truncated=truncated,
                interrupted=interrupted,
                repeats=args.repeat,
                sites=sites,
                remote=hosted,
                setup_seconds=args.setup_seconds,
                preflight_seconds=preflight_seconds,
                prepare_seconds=prepare_seconds,
                grader=grader_pin,
                reset=reset_pin,
                specs=specs,
            )
            _write_text(args.out / "report.json", report.model_dump_json(indent=2) + "\n")
            if skipped_rows:
                print(f"skipped {len(skipped_rows)} task-arm pairs the arm cannot be judged on", flush=True)
            print(f"HEADLINE {report.headline}", flush=True)
            print(report.note, flush=True)
    return 0 if not interrupted and not truncated and _run_cleanly(attempts) and bool(report.paired_tasks) else 1


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main(sys.argv[1:])))
