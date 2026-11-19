"""`fastbrowse --desktop` as a Parallax arm: the apps of a Hyprland desktop task, driven through deskpilot in the box.

The task's apps are the commands the arm may start (`launch_by = "harness"`), so the box starts none and fastbrowse
launches each one itself, on the agent's own hidden workspace. One process per turn, as `SubprocessHarness` runs it:
`fastbrowse --desktop APP... --json`, and its result is read the way `desktop/bench.py` reads it.
"""

from __future__ import annotations

import shlex
from collections.abc import Mapping
from pathlib import Path
from typing import Any

from parallax.harnesses.base import HarnessError, SubprocessHarness
from parallax.harnesses.manifest import ManifestHarness, load_manifest
from parallax.models import (
    ENVIRONMENT_INTERRUPTION,
    Capability,
    CostSource,
    HarnessInfo,
    HarnessKind,
    Step,
    StepKind,
    Task,
    Trial,
    TurnInput,
    TurnOutput,
    TurnStatus,
    Usage,
)

from fastbrowse_evals.backend import OPENROUTER_KEY_ENV
from fastbrowse_evals.desktop.result import cost_lines, read_result
from fastbrowse_evals.evidence import result_answer, step_action

# The release binary is a static-enough linux-x64 build, so the install works in the Arch desktop image with nothing
# but curl and tar. The version is the one the plugin's evals were last checked against.
RELEASE = "v0.11.2"
INSTALL = (
    "curl -fsSL https://github.com/agent-labs-dev/fastbrowse/releases/download/"
    f"{RELEASE}/fastbrowse-rust-linux-x64.tar.gz | tar -xz -C /usr/local/bin fastbrowse"
)
TRACE = "fastbrowse-trace.jsonl"

INFO = HarnessInfo(
    name="fastbrowse-desktop",
    kind=HarnessKind.OPEN,
    capabilities=frozenset({Capability.DESKTOP, Capability.WORKSPACE, Capability.MCP}),
    pins_model=False,
    # fastbrowse's own models serve it and it reports its own spend, which Parallax's meter never sees.
    cost_source=CostSource.HARNESS,
    install=INSTALL,
    version_cmd=("fastbrowse", "--version"),
    credential_env=(OPENROUTER_KEY_ENV,),
    source_url="https://github.com/agent-labs-dev/fastbrowse",
)


class FastbrowseDesktopHarness(SubprocessHarness):
    def __init__(self) -> None:
        super().__init__(INFO)
        self._authorize = False
        self._parsed: TurnOutput | None = None

    def bind_task(self, task: Task) -> None:
        desktop = task.meta.desktop
        if desktop is None or any(not app.command for app in desktop.apps):
            raise HarnessError(f"{INFO.name}: {task.id} must declare desktop apps that carry a command to start")
        self._authorize = task.meta.authorizes

    async def write_config(self, trial: Trial) -> None:
        return None

    def argv(self, turn: TurnInput, *, continuing: bool, session_id: str | None, prompt_file: Path) -> list[str]:
        argv = ["fastbrowse"]
        # The commands the task leaves the harness to start, one argv each, as Parallax hands them to the trial.
        for command in self.trial.desktop_commands:
            argv += ["--desktop", shlex.join(command)]
        argv += ["--json", "--trace", str(self.trial.home / TRACE)]
        if self._authorize:
            argv.append("--authorize")
        return [*argv, "--", turn.text]

    def stdin(self, turn: TurnInput) -> bytes | None:
        return None

    def env(self) -> dict[str, str]:
        env = super().env()
        # fastbrowse reads the key as OPENROUTER_API_KEY; Parallax keeps that name for its meter, so the arm's key is
        # forwarded under its own and renamed here, inside the box. Shown acts where the box's recording can see it,
        # with deskpilot's agent cursor, on a desktop that is the box's own.
        if key := env.pop(OPENROUTER_KEY_ENV, None):
            env["OPENROUTER_API_KEY"] = key
        env["FASTBROWSE_DESKTOP_SHOWN"] = "1"
        return env

    def parse(self, stdout: str, stderr: str) -> TurnOutput:
        result = read_result(stdout)
        self._parsed = turn_output(result, stderr)
        return self._parsed

    async def turn(self, t: TurnInput) -> TurnOutput:
        self._parsed = None
        out = await super().turn(t)
        # Only `complete` exits 0; every other status is the product's outcome, judged from the workspace like a
        # complete one, so a non-zero exit with a result is never a harness failure.
        if self._parsed is not None and self._parsed.status is TurnStatus.COMPLETED and out.error:
            return self.stamp(self._parsed, t.index)
        return out


def turn_output(result: Mapping[str, Any], stderr: str) -> TurnOutput:
    """The turn a fastbrowse result reports. No result is a harness failure, as is a status that says the run never
    reached the apps; an outage of the model provider is the environment's, not the arm's."""
    if not result:
        error = f"fastbrowse printed no result: {stderr.strip()[-400:] or 'no stderr'}"
        return TurnOutput(status=TurnStatus.HARNESS_ERROR, answer="", session_id=None, error=error)
    status = result["status"]
    usage = tuple(
        Usage(
            model=line.get("model") or "",
            component=line["component"],
            input=line.get("input_tokens") or 0,
            output=line.get("output_tokens") or 0,
            cache_read=line.get("cache_read_tokens") or 0,
            cache_write=line.get("cache_write_tokens") or 0,
            requests=line.get("physical_requests") or 0,
            cost_usd=line.get("dollars"),
            source=CostSource.HARNESS,
        )
        for line in cost_lines(dict(result))
        if line.get("component") in ("llm", "jev")
    )
    models = tuple(dict.fromkeys(u.model for u in usage if u.model))
    answer = result_answer(result.get("answer"), result.get("data")) or ""
    if status == "unavailable":
        status_, error = TurnStatus.HARNESS_ERROR, ENVIRONMENT_INTERRUPTION + str(result.get("error"))
    elif status == "error":
        status_, error = TurnStatus.HARNESS_ERROR, str(result.get("error"))
    else:
        status_, error = TurnStatus.COMPLETED, None
    return TurnOutput(
        status=status_,
        answer=answer,
        error=error,
        session_id=None,
        usage=usage,
        actual_models=models,
        steps=steps(result.get("steps") or []),
    )


def steps(items: list[Mapping[str, Any]]) -> tuple[Step, ...]:
    """Each step the run took, as a tool call, with its elapsed time summed as the step's end."""
    out: list[Step] = []
    elapsed = 0
    for item in items:
        elapsed += int(item.get("duration_ms") or 0)
        outcome = item.get("outcome")
        out.append(
            Step(
                turn=0,
                kind=StepKind.TOOL_CALL,
                name=str(item.get("operation")),
                args=item.get("target"),
                output=step_action(str(item.get("operation")), outcome, item.get("note")),
                ok=outcome == "executed",
                t_ms=elapsed,
            )
        )
    return tuple(out)


def claude_code_deskpilot() -> ManifestHarness:
    """claude-code with deskpilot's MCP server: the baseline arm beside `fastbrowse-desktop`."""
    return ManifestHarness(load_manifest(Path(__file__).with_name("claude-code-deskpilot.toml")))
