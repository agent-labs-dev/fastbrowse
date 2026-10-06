"""A watcher that samples the site's own state beside one arm attempt, for `native_state` supplements.

The observer is a command the caller reviewed and pinned by the digest of its code, run through the same trusted
seam as the grader and the reset (no shell, the grader's allowlisted environment, a bounded timeout, a killed
process group). It is read once before the arm starts and then periodically while the arm runs, and it never
sees the agent's answer or outcome: stdin carries the pinned task and the phase only, and stdout is a mapping of
observer name to native reading. The code digest is rechecked before every invocation, so an observer edited after
it was pinned stops sampling instead of continuing under the reviewed digest.

A sample that cannot be read is an error recorded beside the samples, never a fabricated reading: a missing
`before` or `during` leaves the attempt ungraded under a strict supplement. The count of samples is bounded, so a
long attempt cannot grow the retained record without limit.
"""

import asyncio
import json
import math
import re
import shutil
import time
from collections.abc import Sequence
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, JsonValue, TypeAdapter, model_validator

from fastbrowse.evals import external_grade
from fastbrowse.evals.corpus import TaskRef
from fastbrowse.evals.native_state import Sample, TrustedObservations

DEFAULT_INTERVAL = 0.5
MIN_INTERVAL = 0.1
MAX_INTERVAL = 60.0
DEFAULT_TIMEOUT = 10.0
MAX_TIMEOUT = 120.0
MAX_SAMPLES = 600
"""One `before` plus `during` samples. At the default interval this covers ten minutes; a longer attempt is marked
truncated, and a state the sampler never caught leaves the attempt ungraded rather than passed."""
MAX_ERRORS = 50
MAX_OUTPUT_BYTES = 256 * 1024
"""One reading larger than this is an error: a hostile or broken observer must not fill the retained record."""

_HEX = re.compile(r"[a-f0-9]{64}")
_JSON_MAP = TypeAdapter(dict[str, JsonValue])


class ObserverSpec(BaseModel):
    """An observer command the caller reviewed: the argument vector, the file whose digest pins it and its bounds."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    command: tuple[str, ...]
    code: Path
    digest: str = Field(pattern=_HEX.pattern)
    interval: float = DEFAULT_INTERVAL
    timeout: float = DEFAULT_TIMEOUT

    @model_validator(mode="after")
    def _reviewed(self) -> "ObserverSpec":
        if not self.command or any(not isinstance(part, str) or not part for part in self.command):
            raise ValueError("observer command must be a non-empty argument vector of non-empty strings")
        if str(self.code) not in self.command:
            raise ValueError("observer code must name a file the pinned command runs")
        if not math.isfinite(self.interval) or not MIN_INTERVAL <= self.interval <= MAX_INTERVAL:
            raise ValueError(f"observer interval must be between {MIN_INTERVAL:g} and {MAX_INTERVAL:g} seconds")
        if not math.isfinite(self.timeout) or not 0 < self.timeout <= MAX_TIMEOUT:
            raise ValueError(f"observer timeout must be above 0 and at most {MAX_TIMEOUT:g} seconds")
        return self

    def pin(self) -> dict[str, JsonValue]:
        """What a run protocol retains: the program and the code pin, never an argument, which may name a
        credential file or carry a flag value."""
        return {
            "command": [self.command[0]],
            "code": str(self.code),
            "sha256": self.digest,
            "interval": self.interval,
            "timeout": self.timeout,
            "max_samples": MAX_SAMPLES,
        }

    def verify(self) -> None:
        """Raise ValueError unless the code is a file whose digest is the pin and the program can be found."""
        if not self.code.is_file():
            raise ValueError(f"observer code is not a file: {self.code}")
        actual = external_grade.file_digest(self.code)
        if actual != self.digest:
            raise ValueError(f"observer code digest does not match the pinned digest: {actual} != {self.digest}")
        program = self.command[0]
        if shutil.which(program, path=external_grade.safe_environment().get("PATH")) is None:
            raise ValueError(f"observer command not found: {program}")


def build_observer(
    command: str | None,
    *,
    code: Path | None,
    sha256: str | None,
    interval: float | None,
    timeout: float | None,
) -> ObserverSpec | None:
    """The observer the CLI options describe, verified against its pin, or None when none was asked for.

    `command` is a JSON argument vector, so no shell and no quoting rule can reinterpret it."""
    if command is None:
        if code is not None or sha256 is not None or interval is not None or timeout is not None:
            raise ValueError(
                "--observer-code, --observer-sha256, --observer-interval and --observer-timeout need --observer-command"
            )
        return None
    if code is None or sha256 is None:
        raise ValueError("--observer-command requires --observer-code and --observer-sha256 to pin the observer")
    try:
        argv = json.loads(command)
    except ValueError as exc:
        raise ValueError(f"--observer-command must be a JSON array of strings: {exc}") from exc
    if not isinstance(argv, list) or not all(isinstance(part, str) for part in argv):
        raise ValueError("--observer-command must be a JSON array of strings")
    spec = ObserverSpec(
        command=tuple(argv),
        code=code.resolve(),
        digest=sha256,
        interval=DEFAULT_INTERVAL if interval is None else interval,
        timeout=DEFAULT_TIMEOUT if timeout is None else timeout,
    )
    spec.verify()
    return spec


def _reject_constant(name: str) -> object:
    raise ValueError(f"{name} is not JSON")


class StateWatcher:
    """Samples one attempt: `async with` reads `before`, then reads `during` until the block ends.

    Leaving the block cancels the sampler and waits for it, so a running observer process group is killed and no
    periodic task outlives the attempt, even when the attempt is cancelled. Nothing here raises for an observer
    that fails: that is an entry in `errors`."""

    def __init__(self, spec: ObserverSpec, ref: TaskRef) -> None:
        self._spec = spec
        self._ref = ref
        self._samples: list[Sample] = []
        self._errors: list[str] = []
        self._truncated = False
        self._task: asyncio.Task[None] | None = None
        self._began = time.monotonic()

    async def __aenter__(self) -> "StateWatcher":
        self._began = time.monotonic()
        await self._read("before")
        self._task = asyncio.create_task(self._poll(), name="state-watch")
        return self

    async def __aexit__(self, *_: object) -> None:
        task, self._task = self._task, None
        if task is None:
            return
        task.cancel()
        for result in await asyncio.gather(task, return_exceptions=True):
            if isinstance(result, Exception):
                self._error("during", f"sampler stopped: {type(result).__name__}: {result}")

    async def _poll(self) -> None:
        while len(self._samples) < MAX_SAMPLES:
            await asyncio.sleep(self._spec.interval)
            await self._read("during")
        self._truncated = True

    def _error(self, phase: str, message: str) -> None:
        if len(self._errors) < MAX_ERRORS:
            self._errors.append(f"{phase}: {message[:300]}")

    async def _read(self, phase: Literal["before", "during"]) -> None:
        try:
            current = await asyncio.to_thread(external_grade.file_digest, self._spec.code)
            if current != self._spec.digest:
                raise ValueError(f"observer code changed since it was pinned: {current} != {self._spec.digest}")
            request = json.dumps({"task": self._ref.model_dump(mode="json"), "phase": phase}).encode("utf-8")
            stdout = await external_grade._run_command(
                self._spec.command,
                request,
                seconds=self._spec.timeout,
                environment=external_grade.safe_environment(),
            )
            if len(stdout) > MAX_OUTPUT_BYTES:
                raise ValueError(f"observer output exceeded {MAX_OUTPUT_BYTES} bytes")
            data = _JSON_MAP.validate_python(json.loads(stdout, parse_constant=_reject_constant))
        except Exception as exc:  # one failed read is that sample's error; cancellation is not an Exception
            self._error(phase, f"{type(exc).__name__}: {exc}")
            return
        self._samples.append(
            Sample(
                sequence=len(self._samples),
                at=round(time.monotonic() - self._began, 3),
                phase=phase,
                data=data,
            )
        )

    @property
    def samples(self) -> Sequence[Sample]:
        return tuple(self._samples)

    @property
    def errors(self) -> Sequence[str]:
        return tuple(self._errors)

    def trusted(self) -> TrustedObservations | None:
        """The harness's payload for the grader, or None when nothing was read."""
        if not self._samples:
            return None
        return TrustedObservations(
            observer_sha256=self._spec.digest, task_id=self._ref.id, samples=tuple(self._samples)
        )

    def record(self) -> dict[str, JsonValue]:
        """Everything retained beside the attempt artifact: the pin, every sample and every error."""
        return {
            "observer": self._spec.pin(),
            "task": self._ref.id,
            "samples": [sample.model_dump(mode="json") for sample in self._samples],
            "errors": list(self._errors),
            "truncated": self._truncated,
        }
