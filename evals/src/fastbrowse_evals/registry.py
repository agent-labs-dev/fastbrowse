"""Benchmark identity is separate from an agent's brand and from a reported grade."""

from enum import StrEnum
from typing import Literal

from pydantic import BaseModel, ConfigDict


class Suite(StrEnum):
    INTERNAL = "fastbrowse-internal"
    BU_BENCH = "bu-bench"
    MIND2WEB = "online-mind2web"
    WINDTUNNEL = "windtunnel"


class Benchmark(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)

    family: Literal["browser-use"] = "browser-use"
    suite: Suite
    origin: Literal["fastbrowse-internal", "upstream"]
    metric: Literal["source-task-success", "weighted-rubric-score", "full-task-success"]
    arms: tuple[str, ...]
    upstream: str | None


BENCHMARKS = {
    Suite.INTERNAL: Benchmark(
        suite=Suite.INTERNAL,
        origin="fastbrowse-internal",
        metric="source-task-success",
        arms=(
            "fastbrowse",
            "fastbrowse-pool",
            "fastbrowse-hosted",
            "browser-use",
            "browser-use-ultrafast",
            "cua-codex",
            "browser-harness",
            "browser-use-oss",
        ),
        upstream=None,
    ),
    Suite.BU_BENCH: Benchmark(
        suite=Suite.BU_BENCH,
        origin="upstream",
        metric="weighted-rubric-score",
        arms=("fastbrowse",),
        upstream="https://github.com/browser-use/benchmark",
    ),
    Suite.MIND2WEB: Benchmark(
        suite=Suite.MIND2WEB,
        origin="upstream",
        metric="full-task-success",
        arms=("fastbrowse",),
        upstream="https://github.com/OSU-NLP-Group/Online-Mind2Web",
    ),
    Suite.WINDTUNNEL: Benchmark(
        suite=Suite.WINDTUNNEL,
        origin="upstream",
        metric="source-task-success",
        arms=("fastbrowse",),
        upstream="https://github.com/nekuda-ai/WindTunnel",
    ),
}
