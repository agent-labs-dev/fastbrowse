import json
from collections.abc import Mapping
from datetime import UTC, datetime

import pytest
from pydantic import JsonValue

from fastbrowse.config import Config, Thresholds, TokenBudget
from fastbrowse.jev import Answer, Evaluation, NoulAnswer, Question
from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import CostBasis, CostComponent, CostLine, FactReader, Operation, StepOutcome
from fastbrowse.page import Control, Observation
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.policy import HistoryEntry
from fastbrowse.retrieval import claim_check_questions, compose
from fastbrowse.verification import DoneVerdict, check_claims, check_done, llm_verify, page_state
from tests.test_memory import evidence
from tests.test_retrieval import ScriptedLLM

_OPEN = Plan(
    requirements=(Requirement(id="r1", text="Open the httpx repository.", kind=RequirementKind.ACTION),),
    answer_expected=False,
)
_PAGE = Observation(
    url="https://example.test/encode/httpx",
    title="encode/httpx",
    page_key="k",
    captured_at=datetime.now(UTC),
    controls=(),
    omitted_controls=0,
    viewport_text="encode/httpx",
    tabs=(),
)


class _Jev:
    def __init__(self, answers: Mapping[str, float]) -> None:
        self.answers = answers
        self.state: JsonValue = None
        self.questions: Mapping[str, Question] = {}
        self.requests: list[tuple[JsonValue, Mapping[str, Question]]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.state, self.questions = state, questions
        self.requests.append((state, questions))
        answers: dict[str, Answer] = {k: NoulAnswer(probability=p) for k, p in self.answers.items() if k in questions}
        cost = CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0)
        return Evaluation(model="test", answers=answers, input_tokens=1, cost=cost)


# Answers Jev gave live: right pages scored 0.49 to 0.91 complete, near misses 0.04 or less.
@pytest.mark.parametrize(
    ("answers", "verdict"),
    [
        ({"complete": 0.63, "unmet_r1": 0.19}, DoneVerdict.ACCEPT),  # the repository, doubted as a whole
        ({"complete": 0.90, "unmet_r1": 0.14}, DoneVerdict.ACCEPT),
        ({"complete": 0.03, "unmet_r1": 0.85}, DoneVerdict.VERIFY),  # the organisation page
        ({"complete": 0.49, "unmet_r1": 0.66}, DoneVerdict.VERIFY),  # doubt either way goes to the verifier
        ({"complete": 0.72, "unmet_r1": 0.45}, DoneVerdict.VERIFY),
        ({"complete": 0.72}, DoneVerdict.VERIFY),  # an answer Jev did not give confirms nothing
    ],
)
async def test_confirmed_requirements_accept_a_doubted_page(answers: dict[str, float], verdict: DoneVerdict) -> None:
    check = await check_done(_Jev(answers), "Open the httpx repository.", _OPEN, _PAGE, Notes(), Thresholds())
    assert check.verdict is verdict


# A lookup's evidenced requirements are confirmed one by one, as an action's are: pypi-newer's right pages scored
# 0.49 to 0.91 complete, and every one went to the verifier.
@pytest.mark.parametrize(
    ("answers", "verdict"),
    [
        ({"complete": 0.55, "unmet_r1": 0.12}, DoneVerdict.ACCEPT),
        ({"complete": 0.55, "unmet_r1": 0.52}, DoneVerdict.VERIFY),  # the evidence may be about something else
        ({"complete": 0.40, "unmet_r1": 0.05}, DoneVerdict.VERIFY),  # the holistic floor still holds
    ],
)
async def test_confirmed_lookups_accept_below_the_holistic_bar(answers: dict[str, float], verdict: DoneVerdict) -> None:
    total = "Checkout total is $42"
    notes = Notes(
        [
            Fact(
                reader=FactReader.LLM,
                requirement_id="r1",
                text=total,
                evidence=evidence(sha="total", end=len(total)).model_copy(update={"quote": total}),
            )
        ]
    )
    plan = Plan(
        requirements=(Requirement(id="r1", text="Report the checkout total", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    check = await check_done(_Jev(answers), "Total?", plan, _PAGE, notes, Thresholds())
    assert check.verdict is verdict


async def test_a_task_with_nothing_to_do_keeps_the_verifier() -> None:
    plan = Plan(requirements=(), answer_expected=False)
    check = await check_done(_Jev({"complete": 0.7}), "Look around.", plan, _PAGE, Notes(), Thresholds())
    assert check.verdict is DoneVerdict.VERIFY


@pytest.mark.parametrize(("largest", "total"), [(1500, 5000), (5000, 1500)])
async def test_verdict_prompts_keep_late_requirement_evidence_when_notes_overflow(largest: int, total: int) -> None:
    tokens = TokenBudget(state_plus_largest_question=largest, state_plus_all_questions=total)
    notes = Notes(
        Fact(reader=FactReader.LLM, text="Background " * 100, evidence=evidence(sha=f"context-{i}")) for i in range(20)
    )
    total_text = "Checkout total is $42"
    late = Fact(
        reader=FactReader.LLM,
        requirement_id="r1",
        text=total_text,
        evidence=evidence(sha="late", end=len(total_text)).model_copy(update={"quote": total_text}),
    )
    notes.add(late)
    plan = Plan(
        requirements=(Requirement(id="r1", text="Report the checkout total", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    jev = _Jev({"complete": 0.9})
    assert (
        await check_done(jev, "Total?", plan, _PAGE, notes, Thresholds(), tokens=tokens)
    ).verdict is DoneVerdict.ACCEPT
    llm = ScriptedLLM(
        [
            {"complete": True, "missing": []},
            {"claims": [{"text": late.text, "evidence_ids": [fact_id(late)]}]},
        ]
    )
    await llm_verify(llm, "Total?", plan, _PAGE, (), notes, (), config=Config(tokens=tokens))
    composed = await compose(llm, "Total?", plan, notes, tokens=tokens)
    questions = claim_check_questions(composed.data, notes, tokens=tokens)
    for prompt in [
        json.dumps(jev.state),
        llm.calls[0][1][-1].content,
        questions["requirement_omitted"].instructions,
    ]:
        assert late.text in prompt and fact_id(late) in prompt
        assert "facts omitted]" in prompt
    composer_prompt = llm.calls[1][1][-1].content
    assert late.text in composer_prompt and "[e20]" in composer_prompt
    assert "facts omitted]" in composer_prompt
    checker = _Jev({key: 0.0 for key in questions})
    assert await check_claims(checker, composed.data, notes, Thresholds(), tokens=tokens) == composed.data
    for state, batch in [(jev.state, jev.questions), *checker.requests]:
        state_chars = len(json.dumps(state))
        sizes = [len(q.model_dump_json()) for q in batch.values()]
        assert state_chars + max(sizes) <= largest * tokens.chars_per_token
        assert state_chars + sum(sizes) <= total * tokens.chars_per_token


async def test_a_page_too_long_for_the_evidence_is_cut_rather_than_ending_the_run() -> None:
    tokens = TokenBudget(state_plus_largest_question=1500, state_plus_all_questions=1500)
    total_text = "Checkout total is $42"
    notes = Notes(
        [
            Fact(
                reader=FactReader.LLM,
                requirement_id="r1",
                text=total_text,
                evidence=evidence(sha="total", end=len(total_text)).model_copy(update={"quote": total_text}),
            )
        ]
    )
    page = _PAGE.model_copy(update={"viewport_text": 'Flight "row"\n' * 2000})
    state = page_state(page, notes, tokens)
    assert isinstance(state, dict) and isinstance(state["page"], dict) and isinstance(state["notes"], str)
    assert total_text in state["notes"]
    assert "[Viewport text cut:" in str(state["page"]["text"])
    assert len(json.dumps(state)) <= tokens.state_plus_all_questions * tokens.chars_per_token
    plan = Plan(
        requirements=(Requirement(id="r1", text="Report the checkout total", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    llm = ScriptedLLM([{"complete": True, "missing": []}])
    await llm_verify(llm, "Total?", plan, page, (), notes, (), config=Config(tokens=tokens))
    prompt = llm.calls[0][1][-1].content
    assert total_text in prompt and "[Viewport text cut:" in prompt


def test_controls_without_state_give_way_to_the_evidence_before_the_run_ends() -> None:
    # "View more flights" put hundreds of result rows on the page as controls, and those alone left the done check
    # a 0 character notes budget, ending a run that had its evidence.
    tokens = TokenBudget(state_plus_largest_question=1500, state_plus_all_questions=1500)
    total_text = "Checkout total is $42"
    notes = Notes(
        [
            Fact(
                reader=FactReader.LLM,
                requirement_id="r1",
                text=total_text,
                evidence=evidence(sha="total", end=len(total_text)).model_copy(update={"quote": total_text}),
            )
        ]
    )
    stops = Control(
        id="stops",
        frame_id=None,
        role="checkbox",
        label="Nonstop only",
        operations=frozenset({Operation.CLICK}),
        checked=True,
    )
    rows = tuple(
        Control(
            id=f"row{i}",
            frame_id=None,
            role="button",
            label=f"From {100 + i} US dollars. Nonstop flight",
            operations=frozenset({Operation.CLICK}),
        )
        for i in range(300)
    )
    state = page_state(_PAGE.model_copy(update={"controls": (stops, *rows)}), notes, tokens)
    assert isinstance(state, dict) and isinstance(state["notes"], str)
    assert total_text in state["notes"]
    assert state["controls"] == [{"label": "Nonstop only", "role": "checkbox", "checked": True}]
    assert state["controls_omitted"] == 300


async def test_the_verifier_is_told_where_each_requirement_was_read_and_which_addresses_were_guessed() -> None:
    """The verifier could not tell a search the task asked for from a summary page of the same shape, because
    nothing told it which page a fact came from or that the address had been built from the task."""
    notes = Notes()
    fare = Fact(
        reader=FactReader.LLM,
        requirement_id="r1",
        text="From 727 US dollars",
        evidence=evidence().model_copy(update={"url": "https://flights.test/summary", "quote": "From 727 US dollars"}),
    )
    notes.add(fare)
    plan = Plan(
        requirements=(Requirement(id="r1", text="The cheapest nonstop fare", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    guessed = "https://flights.test/?q=flights+from+london"
    llm = ScriptedLLM([{"complete": True, "missing": [], "ungrounded": ["r1"]}])
    await llm_verify(llm, "Cheapest nonstop?", plan, _PAGE, (), notes, (), invented=[guessed])
    prompt = llm.calls[0][1][-1].content
    assert "https://flights.test/summary" in prompt
    assert guessed in prompt
    # Grounding placed after the address left the page text under "## Addresses this run built from the task".
    page = prompt[prompt.index("## Page\n") :]
    assert page.startswith(f"## Page\n{_PAGE.url}\n")
    # Notes and page text follow the address, page text last, so the stable part of the request stays ahead of it.
    assert page.rstrip().endswith(_PAGE.viewport_text)
    assert guessed not in page
    # google-flights: told only to weigh a built address hardest, the verifier refused a loaded search for how
    # the run reached it. It judges what the page shows, and an answer keeps the terms its value was quoted under.
    instructions = " ".join(message.content for message in llm.calls[0][1])
    assert "judge such a page by what it shows, not by how the run reached it" in instructions
    assert "states the terms a value was quoted under" in instructions


async def test_a_verifier_told_of_no_guessed_address_is_asked_nothing_extra() -> None:
    notes = Notes()
    plan = Plan(
        requirements=(Requirement(id="r1", text="The cheapest nonstop fare", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    llm = ScriptedLLM([{"complete": True, "missing": []}])
    await llm_verify(llm, "Cheapest nonstop?", plan, _PAGE, (), notes, ())
    assert "## Addresses this run built from the task" not in llm.calls[0][1][-1].content


async def test_the_checks_are_shown_the_date_and_what_each_action_typed_and_did() -> None:
    """A correction ("type Ada, go back, make it Adam") is only visible in the order of the values typed, and
    "the next Monday after today" only against today."""
    plan = Plan(
        requirements=(
            Requirement(id="r1", text="Enter Ada, go back and correct it to Adam", kind=RequirementKind.ACTION),
        ),
        answer_expected=False,
    )
    history = (
        HistoryEntry(
            operation=Operation.FILL, target="First Name", outcome=StepOutcome.EXECUTED, page_changed=False, text="Ada"
        ),
        HistoryEntry(operation=Operation.CLICK, target="Back", outcome=StepOutcome.EXECUTED, page_changed=True),
        HistoryEntry(
            operation=Operation.FILL,
            target="First Name",
            outcome=StepOutcome.EXECUTED,
            page_changed=False,
            text="Adam",
            effect="First Name: Ada -> Adam",
        ),
    )
    llm = ScriptedLLM([{"complete": True, "missing": []}])
    await llm_verify(llm, "Sign up", plan, _PAGE, (), Notes(), history)
    prompt = llm.calls[0][1][-1].content
    today = _PAGE.today
    assert f"## Current date\n{today}" in prompt
    steps = prompt[prompt.index("## Steps taken\n") :]
    assert steps.index("fill First Name = 'Ada' -> executed") < steps.index("click Back -> executed")
    assert "fill First Name = 'Adam' -> executed (First Name: Ada -> Adam)" in steps
    jev = _Jev({"complete": 0.9})
    # Jev's check can accept outright, so it sees the record too, not only the page a process ended on.
    await check_done(jev, "Sign up", plan, _PAGE, Notes(), Thresholds(), history=history)
    assert isinstance(jev.state, dict) and jev.state["date"] == today
    assert "fill First Name = 'Ada' -> executed" in str(jev.state["actions"])


async def test_the_checks_see_every_address_the_run_has_been_on_first_where_it_began() -> None:
    """A task's "start at" address can be planned as a requirement, and no action records the page a run began on."""
    start = "https://example.test/encode"
    plan = Plan(
        requirements=(Requirement(id="r1", text=f"Navigate to {start}", kind=RequirementKind.ACTION),),
        answer_expected=False,
    )
    visited = (start, _PAGE.url)
    jev = _Jev({"complete": 0.9})
    await check_done(jev, "Open httpx", plan, _PAGE, Notes(), Thresholds(), visited=visited)
    assert isinstance(jev.state, dict) and jev.state["visited"] == list(visited)
    assert "the first of which is where the run began" in jev.questions["unmet_r1"].instructions
    llm = ScriptedLLM([{"complete": True, "missing": []}])
    await llm_verify(llm, "Open httpx", plan, _PAGE, (), Notes(), (), visited=visited)
    assert f"## Visited addresses\n- {start}\n- {_PAGE.url}\n" in llm.calls[0][1][-1].content


def test_dense_jev_requests_have_a_safety_bound_without_squeezing_llm_evidence() -> None:
    tokens = TokenBudget()
    question = "q" * 2831
    context = "x" * 64275
    assert tokens.remaining_chars(context, [question]) > 0
    assert tokens.remaining_chars(context, [question], jev=True) == 0
    assert tokens.input_chars([question], jev=True) + len(question) == 48000
    small = TokenBudget(state_plus_largest_question=1500, state_plus_all_questions=5000)
    assert small.input_chars(["question"], jev=True) == small.input_chars(["question"])


def test_completion_cuts_dense_page_json_before_sending_a_verdict() -> None:
    text = 'Product "row"\n' * 5000
    question = "q" * 2831
    notes = Notes([Fact(reader=FactReader.LLM, requirement_id="r1", text="Total is 42", evidence=evidence())])
    state = page_state(_PAGE.model_copy(update={"viewport_text": text}), notes, questions=[question])
    assert len(json.dumps(state)) + len(question) <= 48000
    assert "Total is 42" in str(state)


async def test_completion_sheds_optional_draft_before_required_source_quotes() -> None:
    from fastbrowse.retrieval import Claim, assemble_answer

    quote = "Grounded source value " * 650
    fact = Fact(
        reader=FactReader.LLM,
        requirement_id="r1",
        text="Requested value",
        evidence=evidence(end=len(quote)).model_copy(update={"quote": quote}),
    )
    notes = Notes((fact,))
    plan = Plan(
        requirements=(Requirement(id="r1", text="Report the requested value", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    draft = assemble_answer(
        (Claim(text="Long optional prose " * 2000, evidence_ids=(fact_id(fact),)),), notes, plan.requirements
    )
    jev = _Jev({"complete": 0.9, "draft_needs_writing": 0.0})
    result = await check_done(jev, "Report the requested value", plan, _PAGE, notes, Thresholds(), draft)
    assert result.verdict is DoneVerdict.ACCEPT and result.answer is None
    assert isinstance(jev.state, dict) and "draft" not in jev.state
    assert quote in str(jev.state) and "draft_needs_writing" not in jev.questions
    assert len(json.dumps(jev.state)) + max(len(q.model_dump_json()) for q in jev.questions.values()) <= 48000


def test_the_same_text_read_off_the_guess_at_another_fragment_answers_again() -> None:
    """A search routed by fragment was guessed at /search#old and refused, then rebuilt by the run at /search#new
    with byte-identical text. A fact's id ignores the fragment while lineage keeps it, so the fresh reading mapped
    onto the held fact and kept its guessed address, and the requirement stayed open."""
    old = evidence(sha="same").model_copy(update={"url": "https://example.test/search#old", "quote": "Fare $320"})
    new = old.model_copy(update={"url": "https://example.test/search#new"})
    notes = Notes()
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="Fare $320", evidence=old))
    notes.discredit(("r1",), lambda item: item.url.endswith("#old"))
    assert not notes.evidenced("r1")
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="Fare $320", evidence=new))
    assert notes.evidenced("r1")
    assert [item.url for item in notes.supporting_evidence("r1")] == ["https://example.test/search#new"]


def test_the_claim_check_shows_no_held_quote_as_a_claims_source() -> None:
    """Jev's claim check was shown a held quote as the cited source while the audit refused it, so the two checks
    of one claim disagreed about what it rests on."""
    from fastbrowse.retrieval import Claim, ComposedAnswer, claim_check_questions

    quote = evidence(sha="guess").model_copy(update={"quote": "Fare $320"})
    notes = Notes()
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="Fare $320", evidence=quote))
    key = fact_id(Fact(reader=FactReader.LLM, text="", evidence=quote))
    notes.discredit(("r1",), lambda item: item.capture_sha256 == "guess")
    composed = ComposedAnswer(
        answer="$320.",
        linked_answer="$320.",
        claims=(Claim(text="It costs $320.", evidence_ids=(key,)),),
        requirements=(Requirement(id="r1", text="Fare", kind=RequirementKind.INFORMATION),),
    )
    instructions = claim_check_questions(composed, notes, check_omission=False)["unsupported_0"].instructions
    assert f"MISSING: {key}" in instructions and "Fare $320" not in instructions


def _in_set_order(first_is_quote_requirement: bool) -> tuple[str, str]:
    """Two requirement ids a set yields in the chosen order in this process, so both orders of a refusal are tried
    whatever the string hash seed."""
    for n in range(100):
        quoted, concluded = f"q{n}", f"c{n}"
        if (next(iter({quoted, concluded})) == quoted) is first_is_quote_requirement:
            return quoted, concluded
    raise AssertionError("no pair in that order")


@pytest.mark.parametrize("r1_first", [True, False])
def test_a_refusal_of_two_requirements_holds_every_guessed_reading_either_way_round(r1_first: bool) -> None:
    """r1 rests on guessed quote A, r2 on a conclusion over A and guessed quote B, and both were refused at once.
    Holding r1's readings first refused the conclusion, r2's support then came back empty, and B was never held,
    so it could answer r2 again."""
    r1, r2 = _in_set_order(r1_first)
    a = evidence(sha="guess").model_copy(update={"quote": "Fare $320"})
    b = evidence(sha="guess").model_copy(update={"quote": "Departs 08:00", "start": 40, "end": 53})
    notes = Notes()
    notes.add(Fact(reader=FactReader.LLM, requirement_id=r1, text="Fare $320", evidence=a))
    quote_a = fact_id(Fact(reader=FactReader.LLM, text="", evidence=a))
    quote_b = fact_id(Fact(reader=FactReader.LLM, text="", evidence=b))
    notes.add(Fact(reader=FactReader.LLM, text="Departs 08:00", evidence=b))
    notes.add(
        Fact(reader=FactReader.LLM, requirement_id=r2, text="$320 at 08:00", evidence=None, basis=(quote_a, quote_b))
    )
    notes.discredit((r1, r2), lambda item: item.capture_sha256 == "guess")
    assert notes.held(quote_a) and notes.held(quote_b)
    notes.add(Fact(reader=FactReader.LLM, requirement_id=r2, text="Departs 08:00", evidence=b))
    assert not notes.evidenced(r2)


def test_a_hold_that_comes_back_takes_the_requirement_with_it() -> None:
    """Lifted by a reading off the guess, a held quote answered its requirement again; once the run read the guessed
    page again the hold was back, and the requirement still counted as answered by a quote no claim could cite."""
    quote = evidence(sha="guess").model_copy(update={"quote": "Fare $320"})
    notes = Notes()
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="Fare $320", evidence=quote))
    on_guess = [True]
    notes.discredit(("r1",), lambda _: on_guess[0])
    on_guess[0] = False
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="Fare $320", evidence=quote))
    assert notes.evidenced("r1")
    on_guess[0] = True
    assert not notes.evidenced("r1") and not notes.supporting_evidence("r1")


def test_a_quote_refused_for_one_requirement_answers_no_claim_until_read_again_off_the_guess() -> None:
    """One quote answered two requirements, a provider and a price, and was refused for the price as read off a
    guessed page. A hold kept for the price alone let a claim stating both through, since the provider still
    stood on the same quote. A held quote answers nothing; the provider is answered again from a fresh reading."""
    from fastbrowse.retrieval import Claim, ComposedAnswer
    from fastbrowse.verification import _output_context

    def composed(key: str) -> ComposedAnswer:
        return ComposedAnswer(
            answer="Acme Air, $320.",
            linked_answer="Acme Air, $320.",
            claims=(Claim(text="Acme Air sells it for $320.", evidence_ids=(key,)),),
            requirements=tuple(
                Requirement(id=r, text=f"{r} detail", kind=RequirementKind.INFORMATION) for r in ("r1", "r2")
            ),
        )

    quote = evidence(sha="guess").model_copy(update={"quote": "Acme Air $320"})
    notes = Notes()
    for requirement in ("r1", "r2"):
        notes.add(Fact(reader=FactReader.LLM, requirement_id=requirement, text="Acme Air $320", evidence=quote))
    held = fact_id(Fact(reader=FactReader.LLM, text="", evidence=quote))
    notes.discredit(("r1",), lambda item: item.capture_sha256 == "guess")

    assert _output_context(composed(held), notes) is None
    assert not notes.evidenced("r1") and not notes.evidenced("r2")
    # Read again on the same page while still on the guess, the quote stays held for both.
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r2", text="Acme Air $320", evidence=quote))
    assert not notes.evidenced("r2")

    fresh = evidence(sha="fresh").model_copy(update={"quote": "Acme Air"})
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r2", text="Acme Air", evidence=fresh))
    reread = fact_id(Fact(reader=FactReader.LLM, text="", evidence=fresh))
    assert notes.evidenced("r2") and not notes.evidenced("r1")
    context = _output_context(composed(reread), notes)
    assert context is not None and context.claims[0].cited_sources[0].quote == "Acme Air"


def test_a_visual_only_page_sends_the_screenshot() -> None:
    from fastbrowse.verification import visual_only_state

    plain = _PAGE.model_copy(update={"viewport_text": "A paragraph long enough to judge a form from, with words."})
    assert not visual_only_state(plain)
    checked = plain.model_copy(
        update={
            "controls": (
                Control(
                    id="c",
                    frame_id=None,
                    role="checkbox",
                    label="Nonstop only",
                    operations=frozenset({Operation.CLICK}),
                    checked=True,
                ),
            )
        }
    )
    assert visual_only_state(checked)
    sparse = plain.model_copy(update={"viewport_text": "Loading"})
    assert visual_only_state(sparse)


def test_the_verifier_prompt_keeps_the_page_last_and_a_byte_identical_request_is_one_key() -> None:
    from fastbrowse.verification import verify_cache_key, verify_messages

    notes = Notes()
    one = verify_messages("Open the page.", _OPEN, _PAGE, (), notes, ())
    two = verify_messages("Open the page.", _OPEN, _PAGE, (), notes, ())
    assert verify_cache_key(one) == verify_cache_key(two)
    assert one[0].role == "system" and "## Verdict" in one[0].content
    assert one[1].content.index("## Task") < one[1].content.index("## Page")
    assert one[1].content.rstrip().endswith("encode/httpx")
    moved = _PAGE.model_copy(update={"viewport_text": "A different page"})
    assert verify_cache_key(verify_messages("Open the page.", _OPEN, moved, (), notes, ())) != verify_cache_key(one)
    imaged = verify_messages("Open the page.", _OPEN, _PAGE, (b"png",), notes, ())
    assert verify_cache_key(imaged) != verify_cache_key(one)
    assert "screenshot" in imaged[0].content and "screenshot" not in one[0].content
