"""Run one live-eval task through jev-ultrafast, in jev-ultrafast's own environment.

fastbrowse_evals.live starts this with `uv run --no-project --with jev-ultrafast@<commit>`, so the package is
installed as published and none of its code is copied here. It reads one JSON request on stdin and prints one
JSON result on stdout:

    {"start", "goal", "cdp_ws", "max_steps": int or null, "max_seconds": number or null, "record": path or null}

The live harness sends `max_steps: null`, `max_seconds: null` and `max_dollars: null`: no benchmark arm is limited, so
the run stops only when the agent ends it (DONE or BLOCKED) or fails. An integer `max_steps` or a number `max_seconds`
is still honoured for direct callers.

The browser is the caller's: a Browser Use Cloud browser reached through `cdp_ws`, the same kind the fastbrowse arm
drives, so both arms pay the same round trips. The caller passes fastbrowse's selected Jev source: OpenRouter
by default, or TypeSafe directly. OpenRouter uses the TypeSafe protocol and key slot
at the supplied base URL. This arm keeps its own retry policy without failover.
The result also carries every decision, stale failure and provenance; with `record`, per-step controls go to
a `.evidence.json` beside the video. The text helper uses TEXT_MODEL_API_KEY, an OpenRouter key.
"""

import base64
import contextlib
import hashlib
import json
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

RECAST_SECONDS = 1.0
FPS = 25


class Unavailable(RuntimeError):
    """The model provider answered only with overload statuses: the harness runs the task again."""


BROWSER_TRANSPORT = "browser_transport"


def failure_class(exc: BaseException) -> str | None:
    """browser-harness's own IPC timeout, found by class since the runner cannot import its private name at load.

    Only that type: a task-budget timeout or any other error is the agent's result, not an infrastructure event."""
    if any(cls.__name__ == "_IPCResponseTimeout" for cls in type(exc).__mro__):
        return BROWSER_TRANSPORT
    return None


class Unreceipted(Unavailable):
    """The connection dropped before a model request had an answer, so what the request cost is unknown."""


class RunBudgetExceeded(RuntimeError):
    """A request would exceed the caller's recorded per-run dollar limit."""


class Meter:
    """Every model request's metered dollars; a request whose cost the provider did not report is counted."""

    def __init__(
        self,
        jev_price: float = 0.0,
        *,
        max_dollars: float | None = None,
        text_input_price: float = 0.0,
        text_output_price: float = 0.0,
        text_max_output_tokens: int = 16384,
    ) -> None:
        self.jev = 0.0
        self.text = 0.0
        self.jev_price = jev_price
        """Dollars per Jev input token (`fastbrowse_evals.backend.JEV_DOLLARS_PER_INPUT_TOKEN`), for a request
        metered at $0. Zero means no list price was given, and such a request's cost is unknown."""
        self.unmetered = 0
        self.max_dollars = max_dollars
        self.text_input_price, self.text_output_price = text_input_price, text_output_price
        self.text_max_output_tokens = text_max_output_tokens
        self.reserved = 0.0
        self.last_reservation = 0.0
        if max_dollars is not None and any(
            not math.isfinite(n) or n <= 0
            for n in (max_dollars, jev_price, text_input_price, text_output_price, text_max_output_tokens)
        ):
            raise ValueError("bounded requests require verified positive prices and ceilings")

    def reserve(self, body: dict[str, Any], kind: str) -> None:
        if self.max_dollars is None:
            return
        # One input token cannot consume less than one UTF-8 byte; page state contains no image inputs.
        size = len(json.dumps(body, ensure_ascii=False).encode())
        charge = size * (self.jev_price if kind == "jev" else self.text_input_price)
        if kind != "jev":
            charge += body["max_tokens"] * self.text_output_price
        if self.dollars + self.reserved + charge > self.max_dollars:
            raise RunBudgetExceeded("the next model request exceeds the run budget")
        self.reserved += charge
        self.unmetered += 1
        self.last_reservation = charge

    @property
    def dollars(self) -> float:
        return self.jev + self.text


def patch_transport(model: Any, meter: Meter) -> None:
    base_url = os.environ.get("FASTBROWSE_JEV_BASE_URL", "").rstrip("/")

    def post_json(url: str, key: str, body: dict[str, Any]) -> dict[str, Any]:
        if "api.typesafe.ai" not in url:
            # The text helper's model wraps its JSON in a stray code fence despite `json_object`, and upstream's
            # strict parse then types nothing: the fence is dropped so the arm is graded on what it chose to type.
            result = ask(url, key, body)
            for choice in result.get("choices") or []:
                message = choice.get("message") or {}
                if isinstance(message.get("content"), str):
                    message["content"] = unfenced(message["content"])
            return result
        # Jev refuses a choice of one option, so it is answered here, as fastbrowse's `asking_open` does. The
        # operation choice always offers DONE and BLOCKED, so a question is always left to send.
        questions = body["questions"]
        forced = {
            name: {"choice": only, "probabilities": {only: 1.0}, "confidence": 1.0}
            for name, question in questions.items()
            if question.get("type") == "choice" and len(question.get("criteria") or {}) == 1
            for only in question["criteria"]
        }
        rest = {name: question for name, question in questions.items() if name not in forced}
        result = ask(url, key, {**body, "questions": rest})
        return {**result, "answers": {**result.get("answers", {}), **forced}}

    def send(url: str, headers: dict[str, str], body: dict[str, Any], kind: str) -> dict[str, Any]:
        if meter.max_dollars is not None:
            return _post(model, url, headers, body, budget=meter, kind=kind)
        try:
            return _post(model, url, headers, body)
        except Unreceipted:
            meter.unmetered += 1
            raise

    def ask(url: str, key: str, body: dict[str, Any]) -> dict[str, Any]:
        kind = "jev" if "api.typesafe.ai" in url else "text"
        if kind == "jev":
            if base_url:
                url = f"{base_url}/v1/systemone"
            if model_id := os.environ.get("FASTBROWSE_JEV_MODEL"):
                body = {**body, "model": model_id}
        if kind == "text" and meter.max_dollars is not None:
            ceiling = body.get("max_tokens", meter.text_max_output_tokens)
            if isinstance(ceiling, bool) or not isinstance(ceiling, int) or ceiling < 1:
                raise ValueError("text helper needs an integer completion ceiling")
            body = {**body, "max_tokens": min(ceiling, meter.text_max_output_tokens)}
        result = send(url, {"Authorization": f"Bearer {key}"}, body, kind)
        usage = result.get("usage") or {}
        _meter(meter, kind, usage.get("cost"), usage.get("inputTokens", usage.get("input_tokens")))
        return result

    model.post_json = post_json


def unfenced(content: str) -> str:
    text = content.strip().removeprefix("```json").removeprefix("```").removesuffix("```")
    return text.strip()


def _meter(meter: Meter, kind: str, cost: object, tokens: object = None) -> None:
    if not isinstance(cost, int | float | str):
        if meter.max_dollars is None:
            meter.unmetered += 1
        return
    if kind == "jev" and not float(cost):
        # A provider can report $0 while tokens flow; priced at list, as fastbrowse prices its own requests. Without
        # the tokens or the price, $0 says nothing about the charge.
        if not (isinstance(tokens, int) and tokens > 0 and meter.jev_price):
            if meter.max_dollars is None:
                meter.unmetered += 1
            return
        cost = tokens * meter.jev_price
    if not math.isfinite(float(cost)) or float(cost) < 0:
        if meter.max_dollars is None:
            meter.unmetered += 1
        return
    if meter.last_reservation:
        meter.unmetered -= 1
    meter.reserved -= meter.last_reservation
    meter.last_reservation = 0.0
    if kind == "jev":
        meter.jev += float(cost)
    else:
        meter.text += float(cost)


RETRYABLE = frozenset({408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529})
"""fastbrowse's own set (`fastbrowse_evals.backend.RETRYABLE_STATUS`): a status that says nothing about the request."""


def _post(
    model: Any,
    url: str,
    headers: dict[str, str],
    body: dict[str, Any],
    *,
    budget: Meter | None = None,
    kind: str = "text",
) -> dict[str, Any]:
    """jev-ultrafast's `post_json`, retry policy unchanged, with an outage raised as `Unavailable` so the harness
    runs the task again rather than counting it: upstream raises one RuntimeError for every failure."""
    import httpx  # jev-ultrafast's dependency, installed beside it

    for attempt in range(3):
        if budget is not None:
            budget.reserve(body, kind)
        try:
            response = model.CLIENT.post(url, json=body, headers=headers)
        except (
            httpx.TimeoutException,
            httpx.NetworkError,
            httpx.RemoteProtocolError,
            httpx.ProxyError,
            httpx.DecodingError,
        ) as error:
            raise Unreceipted(f"Model connection failed ({type(error).__name__}); no action executed.") from None
        except httpx.HTTPError:
            raise RuntimeError("Model connection failed; no action executed.") from None
        if response.status_code in {429, 529, 503} and attempt < 2:
            time.sleep(0.5 * 2**attempt)
            continue
        if response.is_error:
            transient = response.status_code in RETRYABLE
            try:
                error_body = response.json()
            except ValueError:
                error_body = None
            if isinstance(error_body, dict) and isinstance(error := error_body.get("error"), dict):
                param = error.get("param")
                transient |= error.get("isRetryable") is True or (
                    isinstance(param, dict) and param.get("isRetryable") is True
                )
            failed = Unavailable if transient else RuntimeError
            raise failed(f"Model provider returned HTTP {response.status_code}; no action executed.")
        payload = response.json()
        # OpenRouter can hold a request open, then send its error in a 200: that body is no answer to act on.
        if isinstance(payload, dict) and isinstance(error := payload.get("error"), dict):
            code = error.get("code")
            failed = Unavailable if code in RETRYABLE else RuntimeError
            raise failed(f"Model provider returned error {code} in HTTP {response.status_code}; no action executed.")
        return payload
    raise Unavailable("Model unavailable")


class Trace:
    """What upstream decided and why it did not act, read off its state without changing what it does."""

    def __init__(self, agent: Any) -> None:
        self.stale: list[dict[str, Any]] = []
        self.offered: list[dict[str, Any]] = []
        self.last_exception: str | None = None
        self._agent = agent
        self._command = agent.command
        agent.command = self.command

    def command(self, name: str, body: dict[str, Any] | None = None) -> Any:
        state = self._agent.state
        try:
            result = self._command(name, body)
        except Exception as exc:
            self.last_exception = f"{type(exc).__name__}: {exc}"
            if type(exc).__name__ == "StalePage":
                self.stale.append({"decision": len(state["decisions"]), "phase": name, "reason": str(exc)})
            raise
        if name == "predict":
            # Read after upstream's own predict, so the controls are exactly those the choice was made from.
            page = state["page"]
            self.offered.append(
                {
                    "history": len(state["history"]),
                    "url": page["url"],
                    "title": page.get("title"),
                    "text": page.get("text"),
                    "controls": [
                        {"id": a["id"], "kind": a["kind"], "label": str(a["label"])[:80]} for a in page["actions"]
                    ],
                }
            )
        return result

    def decisions(self) -> list[dict[str, Any]]:
        """Each decision beside the controls offered for it; a stale one has no history entry."""
        chosen = self._agent.state["decisions"]
        history = self._agent.state["history"]
        # A stale decision adds no history entry, so the last decision at each history length is the one that acted.
        acted = {offered["history"]: index for index, offered in enumerate(self.offered)}
        return [
            {
                "operation": d.get("operation"),
                "target": d.get("target"),
                "choice": d.get("choice"),
                "probability": (d.get("probabilities") or {}).get(d.get("choice")),
                "confidence": d.get("confidence"),
                "elapsed_ms": d.get("elapsed_ms"),
                "url": offered["url"],
                "observation": offered["text"],
                "typed": history[offered["history"]].get("text")
                if acted[offered["history"]] == index and offered["history"] < len(history)
                else None,
                "offered": len(offered["controls"]),
            }
            for index, (d, offered) in enumerate(zip(chosen, self.offered, strict=True))
        ]


def _capped(offered: dict[str, Any], limit: int = 100) -> dict[str, Any]:
    return {**offered, "controls": offered["controls"][:limit], "total": len(offered["controls"])}


def provenance() -> dict[str, Any]:
    """The helper, model and runner this result came from, read from the process rather than the harness's intent."""
    from importlib import metadata

    try:
        package = metadata.distribution("jev-ultrafast")
        origin = json.loads(package.read_text("direct_url.json") or "{}")
        upstream = {"version": package.version, "commit": (origin.get("vcs_info") or {}).get("commit_id")}
    except Exception as exc:
        upstream = {"error": type(exc).__name__}
    return {
        "jev_ultrafast": upstream,
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "python": platform.python_version(),
        "jev_model": os.environ.get("FASTBROWSE_JEV_MODEL"),
        "text_model": os.environ.get("TEXT_MODEL"),
    }


class Screencast:
    """Chrome's screencast of the agent's tab, frames kept with the browser's timestamps."""

    def __init__(self, browser: Any, folder: Path) -> None:
        from browser_harness.helpers import drain_events

        self._drain = drain_events
        self._browser = browser
        self._folder = folder
        self._stop = threading.Event()
        self._frames: list[tuple[float, Path]] = []
        self._worker = threading.Thread(target=self._capture, daemon=True)

    def __enter__(self) -> "Screencast":
        self._browser.call("Page.startScreencast", format="jpeg", quality=85)
        self._worker.start()
        return self

    def __exit__(self, *_: object) -> None:
        time.sleep(0.1)
        self._stop.set()
        self._worker.join(timeout=3)
        with contextlib.suppress(Exception):
            self._browser.call("Page.stopScreencast")

    def _capture(self) -> None:
        last = time.monotonic()
        while not self._stop.is_set():
            try:
                events = self._drain()
            except Exception:
                return
            if time.monotonic() - last > RECAST_SECONDS:
                # A cross-site navigation swaps the tab's renderer and can end the cast; restarting costs a frame.
                with contextlib.suppress(Exception):
                    self._browser.call("Page.startScreencast", format="jpeg", quality=85)
                last = time.monotonic()
            for event in events:
                if event.get("method") != "Page.screencastFrame" or event.get("session_id") != self._browser.session:
                    continue
                params = event["params"]
                path = self._folder / f"{len(self._frames):06d}.jpg"
                path.write_bytes(base64.b64decode(params["data"]))
                self._frames.append((float(params["metadata"]["timestamp"]), path))
                last = time.monotonic()
                with contextlib.suppress(Exception):
                    self._browser.call("Page.screencastFrameAck", sessionId=params["sessionId"])
            self._stop.wait(0.015)

    def render(self, path: Path) -> str | None:
        """Write the frames at their real timing; None, or why no video was written."""
        if not self._frames:
            return "the browser sent no frames"
        ffmpeg = shutil.which("ffmpeg")
        if ffmpeg is None:
            return "recording needs ffmpeg on PATH"
        frames = sorted(self._frames)
        lines = []
        for (at, frame), (after, _) in zip(frames, [*frames[1:], (frames[-1][0] + 1 / FPS, None)], strict=True):
            lines += [f"file '{frame}'", f"duration {max(after - at, 1 / FPS):.3f}"]
        lines.append(f"file '{frames[-1][1]}'")
        listing = self._folder / "frames.txt"
        listing.write_text("\n".join(lines) + "\n", encoding="utf-8")
        path.parent.mkdir(parents=True, exist_ok=True)
        done = subprocess.run(
            [
                ffmpeg,
                *("-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i", str(listing)),
                *("-vf", f"fps={FPS},scale=trunc(iw/2)*2:trunc(ih/2)*2", "-c:v", "libx264", "-pix_fmt", "yuv420p"),
                *("-movflags", "+faststart", str(path)),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        return None if done.returncode == 0 else done.stderr.strip()[:400]


LOCATION = "location.href"


def observe_final(browser: Any, evidence_script: str) -> dict[str, Any]:
    """The final document's evidence and address as the run left them, read on the agent's own tab before the daemon
    restart resets its viewport. The harness reads the controls afterwards, through the observer every arm's final
    page goes through (`fastbrowse_evals.final_page`). A failure says so, so the grader never sees a page it did
    not read."""

    def evaluate(expression: str, name: str) -> Any:
        result = browser.call("Runtime.evaluate", expression=expression, returnByValue=True)
        if result.get("exceptionDetails"):
            raise RuntimeError(f"{name} script threw")
        return (result.get("result") or {}).get("value")

    try:
        return {"evidence": evaluate(evidence_script, "evidence"), "url": evaluate(LOCATION, "location"), "error": None}
    except Exception as exc:
        return {"evidence": None, "url": None, "error": f"final observation failed ({type(exc).__name__})"}


STEP_CEILING = 1_000_000
"""What jev-ultrafast's `MAX_STEPS` becomes. The package ships it as a 60-action demo budget (and twice that in model
calls), enforced inside `Agent.call` by raising; no benchmark arm is capped, so no run can reach this ceiling."""

STALL_DECISIONS = 32
"""How many decisions in a row may execute no action before the run is stopped as a decision loop.

A choice normally becomes an action, and a stale page discards one, so two decisions per action is ordinary and is
not what this counts: it counts decisions after which the page was not acted on at all. A run that keeps choosing and
never acts is not progressing, and because the demo budget is lifted nothing upstream would ever end it -- in
campaign 41133404129 `arxiv-open` took ~10,000 such decisions before the provider dropped the connection."""

DECISION_CEILING = 512
"""How many decisions one attempt may take when the harness passes no `max_steps`.

The live harness sends `max_steps` null for every arm, and `lift_demo_budget` makes jev-ultrafast's own budget
1,000,000, so an attempt had no bound at all and only a dropped connection ended the loop above. This is the
backstop for the runs that make progress slowly rather than not at all; it is reported as the row's `decision_cap`
so a capped attempt is never read as an uncapped one."""


def lift_demo_budget(agent_module: Any) -> None:
    """Replace jev-ultrafast's demo budget with `STEP_CEILING` in the module that defines `Agent`: `agent.py`
    imported `MAX_STEPS` by name, so that module's global is the one its loop reads."""
    agent_module.MAX_STEPS = STEP_CEILING


def run(request: dict[str, Any]) -> dict[str, Any]:
    from jev_ultrafast import Agent, model

    lift_demo_budget(sys.modules[Agent.__module__])

    meter = Meter(
        request.get("jev_dollars_per_input_token", 0.0),
        max_dollars=request.get("max_dollars"),
        text_input_price=request.get("text_input_price", 0.0),
        text_output_price=request.get("text_output_price", 0.0),
        text_max_output_tokens=request.get("text_max_output_tokens", 16384),
    )
    patch_transport(model, meter)
    started = time.monotonic()
    setup_seconds = 0.0
    status, error, agent, state, trace, kind = "error", None, None, None, None, None
    final_page: dict[str, Any] = {"error": "the runner never opened a page"}
    # Set before the run starts: the result reports it even when the browser never opened.
    ceiling = DECISION_CEILING if request["max_steps"] is None else 2 * request["max_steps"]
    try:
        agent = Agent(request["start"], request["goal"])
        trace = Trace(agent)
        # jev-ultrafast opens a background tab so as not to take over the user's Chrome. This browser is its
        # own, and a cloud browser neither paints nor screencasts a tab that is not in front.
        agent.browser.call("Page.bringToFront")
        # The agent's constructor connected and opened the start page: browser setup, off the agent's clock.
        setup_seconds = time.monotonic() - started
        started += setup_seconds
        with tempfile.TemporaryDirectory() as frames:
            cast = Screencast(agent.browser, Path(frames)) if request.get("record") else None
            try:
                if cast is not None:
                    cast.__enter__()
                idle, acted = 0, 0
                for state in agent.run():
                    if state["status"] in {"done", "blocked"}:
                        break
                    if (limit := request.get("max_seconds")) is not None and time.monotonic() - started >= limit:
                        status = "budget_exceeded"
                        break
                    actions, decisions = len(state["history"]), len(state["decisions"])
                    if request["max_steps"] is not None and (actions >= request["max_steps"] or decisions >= ceiling):
                        status = "budget_exceeded"
                        break
                    # A decision loop is ended here, by the arm's own budget, rather than by the model provider
                    # dropping the connection an hour and a half later: a tick that executed no action
                    # is a choice that went nowhere, and enough of them in a row is a loop, not progress.
                    idle = 0 if actions > acted else idle + 1
                    acted = actions
                    if idle >= STALL_DECISIONS:
                        status = "stalled"
                        error = (
                            f"{decisions} decisions, {idle} in a row that executed no action; "
                            f"stopped at the {STALL_DECISIONS}-decision stall ceiling"
                        )
                        break
                    if decisions >= ceiling:
                        status = "stalled"
                        error = f"{decisions} decisions; stopped at the {ceiling}-decision ceiling"
                        break
                state = agent.snapshot()
                if state["status"] in {"done", "blocked"}:
                    status = state["status"]
            except Exception as exc:  # the run's failure is its result, recorded rather than raised
                error = f"{type(exc).__name__}: {exc}"
                kind = failure_class(exc)
                # A model outage is an outage only before the agent has decided anything. Once it has, the run is
                # its own: an Ultrafast run that loops ten thousand stale picks for an hour and a half and is then
                # cut off by a dropped connection measured the agent, and retrying it as an outage repeats the loop.
                # The harness treats a navigation that fails after the agent acted the same way.
                decided = bool(agent is not None and agent.state.get("decisions"))
                status = (
                    "budget_exceeded"
                    if isinstance(exc, RunBudgetExceeded)
                    else "error"
                    if isinstance(exc, Unavailable) and decided and not kind
                    else "unavailable"
                    if isinstance(exc, Unavailable) or kind
                    else status
                )
            seconds = time.monotonic() - started
            if cast is not None:
                cast.__exit__()
                problem = cast.render(Path(request["record"]))
                if problem:
                    print(f"no video: {problem}", file=sys.stderr)
    except Exception as exc:
        error = error or f"{type(exc).__name__}: {exc}"
        kind = kind or failure_class(exc)
        status = "unavailable" if kind and status == "error" else status
        seconds = time.monotonic() - started
    if agent is not None:
        state = agent.state
        # Read here, not by the harness afterwards: main restarts the daemon next, which resets the viewport override.
        # It follows `seconds`, so observing is not charged to the agent.
        final_page = observe_final(agent.browser, request["evidence_script"])
    history = state["history"] if state else []
    decisions = trace.decisions() if trace else []
    text_helpers = sorted({call.get("model") for call in (state or {}).get("text_calls", []) if call.get("model")})
    artifact = None
    if request.get("record") and trace:
        # Kept beside the video, not in the result line: per-step controls are large and only a diagnosis needs them.
        artifact = str(Path(request["record"]).with_suffix(".evidence.json"))
        Path(artifact).parent.mkdir(parents=True, exist_ok=True)
        Path(artifact).write_text(json.dumps({"offered": trace.offered, "history": history}, default=str))
    return {
        "status": status,
        "error": error,
        "failure_class": kind,
        "seconds": round(seconds, 2),
        "setup_seconds": round(setup_seconds, 2),
        "steps": len(state["decisions"]) if state else 0,
        "decisions": len(state["decisions"]) if state else 0,
        "actions": len(history),
        "step_cap": request["max_steps"],
        "step_cap_unit": "actions",
        "decision_cap": ceiling,
        "trace": [f"{h['kind']} {h['action']} -> {'changed' if h['page_changed'] else 'unchanged'}" for h in history],
        "decision_log": decisions,
        "stale": trace.stale if trace else [],
        "last_exception": trace.last_exception if trace else None,
        "last_offered": _capped(trace.offered[-1]) if trace and trace.offered else None,
        "final_page_url": state["page"]["url"] if state else None,
        "final_page": final_page,
        "artifact": artifact,
        "text_helpers": text_helpers,
        "provenance": provenance(),
        "jev_dollars": round(meter.jev, 6),
        "text_dollars": round(meter.text, 6),
        "unmetered_requests": meter.unmetered,
        "text_model": os.environ.get("TEXT_MODEL"),
    }


def main() -> None:
    request = json.loads(sys.stdin.read())
    result = run(request)
    from browser_harness.admin import restart_daemon

    with contextlib.suppress(Exception):
        restart_daemon()
    print(json.dumps(result))


if __name__ == "__main__":
    main()
