"""The efficiency columns a Parallax row carries, and the two rules that keep them honest.

Speed is model throughput over the LLM calls' own seconds, never the run's wall clock, and a run whose
calls recorded no seconds reports unknown rather than a fabricated number.
"""

from fastbrowse.models import CostBasis, CostBreakdown, CostComponent, CostLine, JevPurpose

from fastbrowse_evals.live import efficiency


def line(
    component: CostComponent,
    *,
    tokens_in: int = 0,
    tokens_out: int = 0,
    seconds: float | None = 1.0,
    requests: int | None = 1,
) -> CostLine:
    return CostLine(
        component=component,
        basis=CostBasis.METERED,
        dollars=0.001,
        input_tokens=tokens_in,
        output_tokens=tokens_out,
        seconds=seconds,
        physical_requests=requests,
    )


def test_throughput_is_tokens_over_the_calls_own_seconds() -> None:
    cost = CostBreakdown(
        lines=(
            line(CostComponent.LLM, tokens_in=1000, tokens_out=500, seconds=1.5),
            line(CostComponent.LLM, tokens_in=1000, tokens_out=500, seconds=0.5),
            line(CostComponent.JEV, tokens_in=800, tokens_out=40, seconds=3.0),
            # Browser time is not a model call and carries no token counts.
            line(CostComponent.BROWSER, seconds=None),
        )
    )

    measured = efficiency(cost)

    assert measured["input_tokens"] == 2000
    assert measured["output_tokens"] == 1000
    assert measured["llm_calls"] == 2
    assert measured["jev_calls"] == 1
    # 3000 tokens over the two LLM calls' own 2.0s, not over the browser's or the judge's seconds.
    assert measured["tokens_per_second"] == 1500.0


def test_no_recorded_call_seconds_reports_unknown_not_zero() -> None:
    cost = CostBreakdown(lines=(line(CostComponent.LLM, tokens_in=900, tokens_out=100, seconds=None),))

    measured = efficiency(cost)

    assert measured["input_tokens"] == 900
    assert measured["llm_calls"] == 1
    assert measured["tokens_per_second"] is None


def test_calls_without_token_counts_still_report_their_counts() -> None:
    cost = CostBreakdown(lines=(line(CostComponent.LLM, seconds=0.5), line(CostComponent.JEV, seconds=0.2)))

    measured = efficiency(cost)

    assert measured["input_tokens"] == 0
    assert measured["output_tokens"] == 0
    assert measured["llm_calls"] == 1
    assert measured["jev_calls"] == 1
    assert measured["tokens_per_second"] == 0.0


def test_throughput_leaves_out_the_tokens_of_a_call_with_no_seconds() -> None:
    cost = CostBreakdown(
        lines=(
            line(CostComponent.LLM, tokens_in=100, seconds=2.0),
            line(CostComponent.LLM, tokens_in=900, seconds=None),
        )
    )
    assert efficiency(cost)["tokens_per_second"] == 50.0


def test_physical_retries_are_counted_instead_of_cost_lines() -> None:
    measured = efficiency(CostBreakdown(lines=(line(CostComponent.LLM, requests=3),)))
    assert measured["llm_calls"] == 3
    assert measured["llm_calls_basis"] == "physical_requests"


def test_missing_physical_receipt_keeps_mixed_count_unknown() -> None:
    measured = efficiency(
        CostBreakdown(lines=(line(CostComponent.LLM, requests=2), line(CostComponent.LLM, requests=None)))
    )
    assert measured["llm_calls"] is None
    assert measured["llm_calls_basis"] == "physical_requests"


def test_no_llm_requests_has_known_zero_count() -> None:
    assert efficiency(CostBreakdown())["llm_calls"] == 0


def test_jev_calls_count_each_request_by_its_purpose() -> None:
    action = line(CostComponent.JEV, requests=2).model_copy(update={"decision_purpose": JevPurpose.ACTION})
    verify = line(CostComponent.JEV).model_copy(update={"decision_purpose": JevPurpose.VERIFY})
    # A historical receipt records no request count, and counts as the one request it was.
    unnamed = line(CostComponent.JEV, requests=None)

    measured = efficiency(CostBreakdown(lines=(action, verify, unnamed, line(CostComponent.LLM))))

    assert measured["jev_calls_by_purpose"] == {"action": 2, "verify": 1, "unnamed": 1}
    assert measured["jev_calls"] == 4
