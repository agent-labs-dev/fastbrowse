"""Estimate API-equivalent token cost without changing the recorded spend ledger."""

import hashlib
import json
from pathlib import Path
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator

TokenCount = Annotated[int, Field(ge=0)]


class ApiCostEstimate(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)

    model: Literal["gpt-6-astra"] = "gpt-6-astra"
    basis: Literal["recorded-token-api-equivalent-range"] = "recorded-token-api-equivalent-range"
    rate_source: Literal["https://developers.openai.com/api/docs/models/gpt-6-astra"] = (
        "https://developers.openai.com/api/docs/models/gpt-6-astra"
    )
    rates_checked: Literal["2026-10-08"] = "2026-10-08"
    usage_sha256: Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
    input_tokens: TokenCount
    cached_input_tokens: TokenCount
    cache_write_input_tokens: TokenCount
    output_tokens: TokenCount
    # Reasoning tokens are already included in output_tokens, so they are not charged twice.
    reasoning_output_tokens: TokenCount
    dollars_low: Annotated[float, Field(ge=0, allow_inf_nan=False)]
    dollars_high: Annotated[float, Field(ge=0, allow_inf_nan=False)]

    @model_validator(mode="after")
    def agrees_with_usage(self) -> "ApiCostEstimate":
        if self.cached_input_tokens + self.cache_write_input_tokens > self.input_tokens:
            raise ValueError("cached and written tokens exceed input tokens")
        if self.reasoning_output_tokens > self.output_tokens:
            raise ValueError("reasoning tokens exceed output tokens")
        low, high = token_cost_range(
            self.input_tokens, self.cached_input_tokens, self.cache_write_input_tokens, self.output_tokens
        )
        if abs(self.dollars_low - low) > 1e-9 or abs(self.dollars_high - high) > 1e-9:
            raise ValueError("estimated cost does not match usage and rates")
        return self


def token_cost_range(inputs: int, cached: int, written: int, outputs: int) -> tuple[float, float]:
    uncached = inputs - cached - written
    low = (uncached * 10 + cached + written * 12.5 + outputs * 50) / 1_000_000
    # CLI totals combine requests, so a large total cannot prove which requests used long-context rates.
    high = (uncached * 20 + cached * 2 + written * 25 + outputs * 75) / 1_000_000 if inputs > 272_000 else low
    return low, high


def recover_api_cost(events: Path, *, model: str, service_tier: str) -> ApiCostEstimate | None:
    """Read one completed CLI turn, refusing absent, ambiguous or unsupported usage."""
    if model != "gpt-6-astra" or service_tier != "default" or not events.is_file():
        return None
    try:
        content = events.read_bytes()
        rows = [json.loads(line) for line in content.splitlines() if line.strip()]
    except (OSError, ValueError):
        return None
    if not all(isinstance(row, dict) for row in rows):
        return None
    completed = [row for row in rows if row.get("type") == "turn.completed"]
    if len(completed) != 1 or any(row.get("type") == "turn.failed" for row in rows):
        return None
    usage = completed[0].get("usage")
    if not isinstance(usage, dict):
        return None
    fields = (
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
    )
    if any(type(usage.get(key)) is not int or usage[key] < 0 for key in fields):
        return None
    if usage["cached_input_tokens"] + usage["cache_write_input_tokens"] > usage["input_tokens"]:
        return None
    if usage["reasoning_output_tokens"] > usage["output_tokens"]:
        return None
    low, high = token_cost_range(*(usage[key] for key in fields[:4]))
    return ApiCostEstimate(
        usage_sha256=hashlib.sha256(content).hexdigest(),
        **{key: usage[key] for key in fields},
        dollars_low=low,
        dollars_high=high,
    )


def main() -> None:
    """Recover usage by exact ledger row and preserve the original source digest."""
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ledger", type=Path, required=True)
    parser.add_argument("--artifact-root", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    content = args.ledger.read_bytes()
    rows = [json.loads(line) for line in content.splitlines() if line.strip()]
    estimates = []
    for index, row in enumerate(rows):
        if row.get("arm") != "cua-codex":
            continue
        artifact = row.get("artifact")
        if not isinstance(artifact, str):
            continue
        directory = (args.artifact_root / artifact).resolve()
        if not directory.is_relative_to(args.artifact_root.resolve()):
            raise ValueError("artifact path leaves its root")
        provenance = row.get("provenance", {})
        estimate = recover_api_cost(
            directory / "events.jsonl",
            model=row.get("model", ""),
            service_tier=provenance.get("service_tier", ""),
        )
        if estimate is not None:
            estimates.append({"row_index": index, "estimated_api_cost": estimate.model_dump(mode="json")})
    args.out.write_text(
        json.dumps({"ledger_sha256": hashlib.sha256(content).hexdigest(), "estimates": estimates}, indent=2) + "\n"
    )


if __name__ == "__main__":
    main()
