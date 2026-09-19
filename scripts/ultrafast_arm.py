"""Run one live-eval task through jev-ultrafast, in jev-ultrafast's own environment.

fastbrowse.evals.live starts this with `uv run --no-project --with jev-ultrafast@<commit>`, so the package is
installed as published and none of its code is copied here. It reads one JSON request on stdin and prints one
JSON result on stdout:

    {"start", "goal", "cdp_ws", "max_steps", "max_dollars", "max_seconds", "record": path or null}

The browser is the caller's: a Browser Use Cloud browser reached through `cdp_ws`, the same kind the fast arm
drives, so both arms pay the same round trips. jev-ultrafast calls TypeSafe's direct API with
TYPESAFE_API_KEY; with only AI_GATEWAY_API_KEY set, the same questions go through the Vercel AI Gateway, which
is also how the fast arm reaches Jev. Its text helper uses TEXT_MODEL_API_KEY, an OpenRouter key.
"""

import base64
import contextlib
import html
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

GATEWAY_URL = "https://ai-gateway.vercel.sh/v4/ai/evaluation-model"
GATEWAY_HEADERS = {
    "ai-gateway-auth-method": "api-key",
    "ai-gateway-protocol-version": "0.0.1",
    "ai-model-id": "typesafe-ai/jev",
    "ai-evaluation-model-specification-version": "4",
}
ROUNDING_UNIT = 0.01
"""The gateway rounds probabilities to hundredths so they sum to one, which can leave Jev's choice a hundredth
below another option. fastbrowse accepts that near-tie; jev-ultrafast, written against unrounded direct-API
answers, would reject the whole response, so the chosen option is lifted to the top before it checks."""
RESULT_SECONDS = 4.0
RECAST_SECONDS = 1.0
FPS = 25


def systemone_answer(payload: dict[str, Any]) -> tuple[dict[str, Any], float | None]:
    """A gateway response in the direct API's shape, and the gateway's metered cost if it reported one."""
    metadata = payload.get("providerMetadata") or {}
    confidence = (metadata.get("typesafe") or {}).get("confidence") or {}
    answers: dict[str, Any] = {}
    for key, raw in (payload.get("answers") or {}).items():
        answer = {**raw, "confidence": confidence.get(key)}
        probabilities = answer.get("probabilities")
        choice = answer.get("choice")
        if isinstance(probabilities, dict) and choice in probabilities:
            top = max(probabilities.values())
            if probabilities[choice] < top and top - probabilities[choice] <= ROUNDING_UNIT + 1e-9:
                answer["probabilities"] = {**probabilities, choice: top}
        answers[key] = answer
    cost = (metadata.get("gateway") or {}).get("cost")
    return (
        {"answers": answers, "model": "typesafe-ai/jev", "usage": payload.get("usage", {})},
        None if cost is None else float(cost),
    )


class Meter:
    """Every model request's metered dollars; a request whose cost the provider did not report is counted."""

    def __init__(self) -> None:
        self.jev = 0.0
        self.text = 0.0
        self.unmetered = 0

    @property
    def dollars(self) -> float:
        return self.jev + self.text


def patch_transport(model: Any, meter: Meter) -> None:
    original = model.post_json
    via_gateway = not os.environ.get("TYPESAFE_API_KEY")
    if via_gateway:
        if not os.environ.get("AI_GATEWAY_API_KEY"):
            raise RuntimeError("set TYPESAFE_API_KEY or AI_GATEWAY_API_KEY for Jev")
        # jev-ultrafast reads its Jev key from here and passes it to post_json, which sends it to the gateway.
        os.environ["TYPESAFE_API_KEY"] = os.environ["AI_GATEWAY_API_KEY"]

    def post_json(url: str, key: str, body: dict[str, Any]) -> dict[str, Any]:
        if "api.typesafe.ai" in url and via_gateway:
            payload = _gateway(model, key, {"state": body["state"], "questions": body["questions"]})
            result, cost = systemone_answer(payload)
            _meter(meter, "jev", cost)
            return result
        result = original(url, key, body)
        usage = result.get("usage") or {}
        _meter(meter, "jev" if "api.typesafe.ai" in url else "text", usage.get("cost"))
        return result

    model.post_json = post_json


def _meter(meter: Meter, kind: str, cost: object) -> None:
    if not isinstance(cost, int | float | str):
        meter.unmetered += 1
        return
    if kind == "jev":
        meter.jev += float(cost)
    else:
        meter.text += float(cost)


def _gateway(model: Any, key: str, body: dict[str, Any]) -> dict[str, Any]:
    # jev-ultrafast's own client and retry policy: three attempts on an overloaded provider.
    for attempt in range(3):
        response = model.CLIENT.post(
            GATEWAY_URL, json=body, headers={"Authorization": f"Bearer {key}", **GATEWAY_HEADERS}
        )
        if response.status_code in {429, 529, 503} and attempt < 2:
            time.sleep(0.5 * 2**attempt)
            continue
        if response.is_error:
            raise RuntimeError(f"Model provider returned HTTP {response.status_code}; no action executed.")
        return response.json()
    raise RuntimeError("Model unavailable")


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

    def __enter__(self) -> Screencast:
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


def show_result(browser: Any, goal: str, headline: str, summary: str) -> None:
    """End the video on the outcome, as fastbrowse's recordings do."""
    card = (
        "<meta charset=utf-8><body style='margin:0;height:100vh;display:grid;place-content:center;gap:28px;"
        "padding:0 8vw;background:#0d1117;color:#e6edf3;font:24px system-ui,sans-serif'>"
        f"<div style='color:#8b949e'>{html.escape(goal)}</div>"
        f"<div style='font-size:40px;font-weight:600'>{html.escape(headline)}</div>"
        f"<div style='color:#3fb950'>{html.escape(summary)}</div></body>"
    )
    browser.call("Page.navigate", url="data:text/html;base64," + base64.b64encode(card.encode()).decode())
    time.sleep(RESULT_SECONDS)


def run(request: dict[str, Any]) -> dict[str, Any]:
    from jev_ultrafast import Agent, model

    meter = Meter()
    patch_transport(model, meter)
    started = time.monotonic()
    status, error, agent, state = "error", None, None, None
    try:
        agent = Agent(request["start"], request["goal"])
        # jev-ultrafast opens a background tab so as not to take over the user's Chrome. This browser is its
        # own, and a cloud browser neither paints nor screencasts a tab that is not in front.
        agent.browser.call("Page.bringToFront")
        with tempfile.TemporaryDirectory() as frames:
            cast = Screencast(agent.browser, Path(frames)) if request.get("record") else None
            try:
                if cast is not None:
                    cast.__enter__()
                for state in agent.run():
                    if state["status"] in {"done", "blocked"}:
                        break
                    if (
                        len(state["decisions"]) >= request["max_steps"]
                        or meter.dollars > request["max_dollars"]
                        or time.monotonic() - started > request["max_seconds"]
                    ):
                        status = "budget_exceeded"
                        break
                state = agent.snapshot()
                if state["status"] in {"done", "blocked"}:
                    status = state["status"]
            except Exception as exc:  # the run's failure is its result, recorded rather than raised
                error = f"{type(exc).__name__}: {exc}"
            seconds = time.monotonic() - started
            if cast is not None:
                steps = len(agent.state["decisions"])
                show_result(
                    agent.browser,
                    request["goal"],
                    {"done": "DONE", "blocked": "BLOCKED"}.get(status, error or status),
                    f"{status} in {seconds:.1f}s, {steps} steps, ${meter.dollars:.4f}",
                )
                cast.__exit__()
                problem = cast.render(Path(request["record"]))
                if problem:
                    print(f"no video: {problem}", file=sys.stderr)
    except Exception as exc:
        error = error or f"{type(exc).__name__}: {exc}"
        seconds = time.monotonic() - started
    final_url, controls = None, None
    if agent is not None:
        state = agent.state
        final_url = state["page"]["url"]
        # The controls it last observed, as the fast arm's harness observes them: for graders that read the form.
        controls = [[a["label"], a.get("value")] for a in state["page"].get("actions", [])]
        agent.close()
    history = state["history"] if state else []
    return {
        "status": status,
        "error": error,
        "seconds": round(seconds, 2),
        "final_url": final_url,
        "controls": controls,
        "steps": len(state["decisions"]) if state else 0,
        "actions": len(history),
        "trace": [f"{h['kind']} {h['action']} -> {'changed' if h['page_changed'] else 'unchanged'}" for h in history],
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
