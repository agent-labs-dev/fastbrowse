"""The run loop: Jev chooses each action, the LLM plans, reads, writes and recovers, and code owns every gate.

Only `Status.COMPLETE` is success. Anything the loop cannot prove (an answer whose claims fail their checks,
a DONE the verifier rejects at the end of the budget) is reported as what it is rather than rounded up.
"""

import asyncio
import hashlib
import json
import logging
import time
from collections import deque
from collections.abc import Coroutine, Mapping, Sequence, Set
from dataclasses import dataclass, field
from urllib.parse import urljoin, urlsplit

from pydantic import BaseModel, Field, JsonValue

from fastbrowse.citations import ANSWER_LINK, text_fragment
from fastbrowse.config import Config, ObservationLimits
from fastbrowse.effects import (
    SETTING_ROLES,
    ControlKey,
    ControlValue,
    Move,
    effect,
    move,
    reversal,
    state_key,
)
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, JevClient, JevError, NoulAnswer, NoulQuestion
from fastbrowse.llm import Generation, LLMClient, LLMError, Message
from fastbrowse.memory import Fact, Notes, NotesTooLarge
from fastbrowse.models import (
    UNTRUSTED,
    Attachment,
    Authorization,
    Citation,
    CostComponent,
    Decider,
    EventHandler,
    Evidence,
    Frozen,
    Limits,
    LLMPurpose,
    Operation,
    RunResult,
    SecretResolver,
    Status,
    StepEvent,
    StepFact,
    StepOutcome,
    StepResult,
    TripwireMode,
    Unavailable,
    UntilCheck,
)
from fastbrowse.page import (
    Action,
    ActResult,
    BrowserError,
    Capture,
    Control,
    Observation,
    Page,
    pager_link,
)
from fastbrowse.planner import Plan, Requirement, RequirementKind, make_plan
from fastbrowse.policy import (
    Decision,
    HistoryEntry,
    ObservationTooLarge,
    ReadAssessment,
    Reduction,
    StepContext,
    decide,
)
from fastbrowse.retrieval import ComposedAnswer, compose, draft_answer, read
from fastbrowse.safety import (
    Redactor,
    irreversible_question,
    may_be_irreversible,
    origin_of,
    resolve_secret,
    secret_allowed,
)
from fastbrowse.shortcut import Shortcut, accept, accept_start, propose_shortcut, propose_start
from fastbrowse.telemetry import BudgetExceeded, Ledger, trace
from fastbrowse.tripwires import Tripped, Tripwire, repeated_action, stagnant_plan
from fastbrowse.verification import (
    DoneVerdict,
    Extraction,
    LLMVerdict,
    check_claims,
    check_done,
    extract,
    llm_verify,
    page_state,
)

GENERATE = "generate"

# Long enough for a browser-verification page to run its check and hand over, short enough that a page
# which never moves still ends as needs_login well inside a run's time budget.
_INTERSTITIAL_SECONDS = 12.0
# How long a shortcut may outlast the start page's load. Flash-lite answers in about 0.8s and a cloud page
# loads in one to two, so a proposal later than this is an outlier costing more than it saves.
_SHORTCUT_GRACE_SECONDS = 1.0
_INTERSTITIAL_POLL_SECONDS = 0.5
_REDRAW_WATCH_SECONDS = 10.0
"""Longer than deciding takes: a watch ends with the work it watches, and this only bounds a stuck one."""
_NOT_ACTING = frozenset({Operation.READ, Operation.DONE})
_REPEATS_BEFORE_CYCLE = 2
"""Times one action may be taken from one page and still count as progress. Scrolling is exempt: a long page
takes many scrolls, each of which shows something new."""
_PAGE_OPERATIONS = frozenset({Operation.READ, Operation.SCROLL, Operation.BACK, Operation.ESCAPE, Operation.DONE})
"""Recovery can direct page operations without a control; directed DONE still requires verification."""
_CYCLE_SHOWN = 4
"""Actions named when a run arrives back at a page state, the most recent last."""
_REVERSAL_WINDOW = 6
_RECOVERY_RECORDS = 4
_RECOVERY_CHARS = 240
_IDLE_CHECKED = frozenset({Operation.CLICK, Operation.ENTER})
"""Operations not taken twice from a page state where they changed nothing. A hover can reveal content through CSS
alone, which leaves the DOM as it was, so it is not judged by the DOM."""

logger = logging.getLogger(__name__)

type _Prepared = ComposedAnswer | asyncio.Task[Generation[ComposedAnswer]] | None
"""An answer ready before conclusion: the reader's facts Jev accepted as written, or a composer in flight."""

type ReadKey = tuple[str, str, tuple[str, ...]]
type Signature = tuple[Operation, str | None, str]
"""One action on one target, from one page state: the key both the cycle count and the no-op memory are kept by."""


class _FieldText(Frozen):
    missing: bool = Field(
        description=(
            "True when the field wants a fact about the user (a phone number, address, account or order id) "
            "that the task and notes neither state nor state a part of. Such a value is never invented."
        )
    )
    text: str = Field(description="Exactly the text to type into the field, with no commentary; empty when missing.")


_FIELD_WRITER = (
    "# Field writer\nWrite only the text for one form field. "
    "Infer its meaning from the task, current value, page context and recent actions. "
    "Use the field's displayed format for dates. "
    "A fact the task states in another shape is given, not missing: take the part of a "
    "stated name, address or date this field asks for and write it in the field's shape.\n\n"
    f"# Trust\n{UNTRUSTED}"
)


class _Recovery(Frozen):
    diagnosis: str
    next_subgoal: str = Field(description="The single next thing to achieve on the page, concretely.")
    control: int | None = Field(
        default=None, description="The index of the one listed control the subgoal acts on, or null if none."
    )
    operation: Operation | None = Field(
        default=None, description="What the subgoal does to that control, or to the page (read, scroll, back, escape)."
    )
    give_up: bool = Field(description="True only when the task cannot progress without the user.")
    needs_input: bool = Field(
        default=False,
        description="With give_up: true when what the user must supply is a value the task never gave, such as a "
        "field it names no value for; false for any other dead end.",
    )


class _Stop(Exception):
    def __init__(self, status: Status, error: str | None = None) -> None:
        super().__init__(error or status.value)
        self.status = status
        self.error = error


class _Unsure(Exception):
    """The next action is authorized but not confidently the right one: a case for recovery, not for the caller."""

    def __init__(self, reason: str, *, gives_up_as: Status = Status.STUCK) -> None:
        super().__init__(reason)
        # How the run ends if recovery finds no other way: a missing value is still the user's to give.
        self.gives_up_as = gives_up_as


@dataclass(slots=True)
class _Attempts:
    """One action on one target, from one page state: how often it was taken, and what the last one did."""

    count: int = 0
    idle: bool = False
    """The last attempt left the page as it was, so taking it again is a loop rather than a retry."""


@dataclass(slots=True)
class _RunState:
    task: str
    inputs: Mapping[str, str]
    attachments: tuple[Attachment, ...]
    authorization: Authorization
    ledger: Ledger
    planning: asyncio.Task[Generation[Plan]]
    notes: Notes = field(default_factory=Notes)
    steps: list[StepResult] = field(default_factory=list[StepResult])
    would_fire: list[Tripwire] = field(default_factory=list[Tripwire])
    history: list[HistoryEntry] = field(default_factory=list[HistoryEntry])
    hint: str | None = None
    directed: tuple[Operation, str | None] | None = None
    """The operation and control id recovery named, taken when Jev is still unsure of the next step."""
    unchanged: int = 0
    written: dict[str, set[str]] = field(default_factory=dict[str, set[str]])
    """Controls a fill or select has written, by document, so only a field's first new value counts as progress by
    itself. A new document restarts control ids, and its fields would otherwise inherit the last page's writes."""
    recoveries: int = 0
    recovery_log: deque[str] = field(default_factory=lambda: deque(maxlen=_RECOVERY_RECORDS))
    recovered_at: int = 0
    """`len(history)` when a tripwire last recovered the run. Evidence a recovery already acted on is
    spent: `history` only grows, so a repetition count that reached the limit once would hold forever and
    re-enter recovery on every later step, whatever the run did next."""
    plan_marks: list[str] = field(default_factory=list[str])
    """One fingerprint of the still-unevidenced requirements per step, for `stagnant_plan`."""
    attempts: dict[Signature, _Attempts] = field(default_factory=dict[Signature, "_Attempts"])
    """What became of each action taken from each page state, which is how a cycle is told from progress."""
    last_page: tuple[str, str] | None = None
    reached: dict[str, int] = field(default_factory=dict[str, int])
    """Each page state the run has been in, with how many actions had been taken when it was first reached. Only
    reaching a new one restores the recovery budget or counts an action that changed the page as progress."""
    left: str | None = None
    """The state the last action that changed the page was taken from, until the next observation judges it."""
    acted_from: Observation | None = None
    """The page the last action was taken on, until the next observation says what it did."""
    pending_move: Move | None = None
    moves: list[tuple[int, Move]] = field(default_factory=list[tuple[int, Move]])
    settings_held: set[tuple[str, frozenset[tuple[ControlKey, ControlValue]]]] = field(
        default_factory=set[tuple[str, frozenset[tuple[ControlKey, ControlValue]]]]
    )
    """Each document's committed control values seen so far, which a setting put back to cannot renew recovery."""
    ready_plan: Plan | None = None
    invented: set[str] = field(default_factory=set[str])
    """Addresses this run built from the task rather than reached by clicking: an accepted shortcut, or a start
    page worked out from the task. These are the ones that can land on a page of the right shape and the wrong
    search, so the verifier is told which they were."""
    owes_read: bool = False
    """An interaction changed the page after the notes last read it. A run clicked a filter and declared itself
    done, and the check judged notes read off the list before the filter applied, so an answer is not finished
    until a read of what the interaction produced has run."""
    read_here: bool = False
    """This page has been read since it last changed."""
    tried_unsure: set[str] = field(default_factory=set[str])
    """Page states where an unsure pick has been acted on instead of recovering; the next one there recovers."""
    reads: set[ReadKey] = field(default_factory=set)
    """Attempted reads by document, exact content and outstanding requirements, independent of URL edits."""
    barren: dict[ReadKey, int] = field(default_factory=dict[ReadKey, int])
    """Reads by document, page state and outstanding requirements that added no fact. Keyed by what can be done
    on the page rather than by its exact text, so a page rewriting itself cannot mint a fresh key for ever."""
    next_page: bool = False
    """Open this page's next page, set when the reader says a list the run needs goes on past the page it read."""
    paged_from: str | None = None
    """The page state a next-page click left, so the page it opens is read without a decision."""
    pages: int = 0
    """Next pages opened by code this run."""
    first_url: str | None = None
    """The first page the run looked at, which is what a task's "this page" means once the run has moved on."""
    redecided: bool = False
    """A decision was dropped because the page redrew under it, so nothing is watched until an action is taken."""
    missing: set[tuple[str, str | None]] = field(default_factory=set[tuple[str, str | None]])
    """Fields the task gives no value for, which recovery has been told about once, by label and the context that
    tells twins apart: a second passenger's frequent-flyer box is not the first one revisited."""
    continuing: set[str] = field(default_factory=set[str])
    """Requirements the reader said range over a list that goes on past the page it read. A scalar choice cannot
    answer one of those, so it is not asked about them again on the next page."""

    @property
    def plan(self) -> Plan:
        """The plan, for code that only runs after `await_plan`: reading, judging DONE and answering."""
        if self.ready_plan is None:
            raise RuntimeError("the plan was used before it was awaited")
        return self.ready_plan

    async def await_plan(self) -> Plan:
        if self.ready_plan is None:
            planned = await self.planning
            self.ledger.record(planned.cost)
            self.ready_plan = planned.data
        return self.ready_plan


class Agent:
    def __init__(
        self,
        page: Page,
        jev: JevClient,
        llm: LLMClient,
        *,
        config: Config | None = None,
        secrets: SecretResolver | None = None,
        on_event: EventHandler | None = None,
    ) -> None:
        self._page = page
        self._jev = jev
        self._llm = llm
        self._config = config or Config()
        self._secrets = secrets
        self._on_event = on_event
        self._redactor = Redactor()
        self._secret_on_screen = False
        self._raw_observation: Observation | None = None
        self._artifact_start = 0

    async def run(
        self,
        task: str,
        *,
        start: str | None = None,
        choose_start: bool = False,
        inputs: Mapping[str, str] | None = None,
        attachments: Sequence[Attachment] = (),
        output_schema: type[BaseModel] | None = None,
        limits: Limits | None = None,
        authorization: Authorization | None = None,
        until: UntilCheck | None = None,
    ) -> RunResult:
        ledger = Ledger(limits or Limits())
        self._artifact_start = len(self._page.artifacts)
        state: _RunState | None = None
        planning: asyncio.Task[Generation[Plan]] | None = None
        # The ledger checks `max_seconds` between operations; only a deadline around the awaits bounds a
        # browser or provider call that never returns.
        deadline = asyncio.timeout(ledger.limits.max_seconds)
        try:
            async with deadline:
                # The plan is needed to read, to judge DONE and to answer, and the start page, the first fills
                # and clicks all come before those, so it is written from the task while they run.
                planning = asyncio.create_task(make_plan(self._llm, task, start=start, ledger=ledger))
                # `start=None` leaves the browser where it is, which is what a caller stepping a run on
                # from a page it opened itself wants. `choose_start` is the other case: a caller with a goal
                # and no page at all, who wants the first address worked out from the task.
                opening = start if start is not None or not choose_start else await self._first_page(task, ledger)
                history, invented = ([], set[str]()) if opening is None else await self._open(task, opening, ledger)
                if start is None and opening is not None:
                    # The caller gave a goal and no page, so this address was worked out from the task too.
                    invented.add(opening)
                    await self._front_page_if_blank(opening)
                state = _RunState(
                    task, inputs or {}, tuple(attachments), authorization or Authorization(), ledger, planning
                )
                state.history.extend(history)
                state.invented = invented
                return await self._loop(state, output_schema, until)
        except _Stop as stop:
            return self._result(state, ledger, stop.status, error=stop.error)
        except TimeoutError:
            if not deadline.expired():
                raise
            limit = f"time limit {ledger.limits.max_seconds}s reached"
            return self._result(state, ledger, Status.BUDGET_EXCEEDED, error=limit)
        except BudgetExceeded as error:
            return self._result(state, ledger, Status.BUDGET_EXCEEDED, error=str(error))
        except (ObservationTooLarge, NotesTooLarge) as error:
            return self._result(state, ledger, Status.OBSERVATION_LIMIT, error=str(error))
        except (JevError, LLMError, BrowserError) as error:
            message = self._redactor.redact(str(error))[:500]
            trace("run_error", kind=type(error).__name__, step=len(state.steps) if state else 0, error=message)
            status = Status.UNAVAILABLE if isinstance(error, Unavailable) else Status.ERROR
            return self._result(state, ledger, status, error=message)
        finally:
            # A run can end before it ever needed the plan, and a plan still being written would bill it.
            if planning is not None:
                await _discard(planning)

    async def _loop(
        self, state: _RunState, output_schema: type[BaseModel] | None, until: UntilCheck | None
    ) -> RunResult:
        while True:
            state.ledger.check()
            observation = await self._observe()
            state.first_url = state.first_url or observation.url
            self._note_effect(state, observation)
            undone, renews, put_back = self._reversal(state)
            stalled = self._settle(state, observation, renews=renews, put_back=put_back)
            if (stalled := undone or stalled) is not None:
                await self._recover(state, observation, stalled)
                continue
            # Walking a list is dispatched before Jev is asked, because not asking is the point: the reader asked for
            # the rest of the list and the link is already found, so a decision would buy nothing. It therefore passes
            # none of the gates below, which is safe only because of what it can be: a pager link to another address.
            # The login check needs a decision Jev has not made yet, and the page it opens is judged on the next turn.
            # The confidence gates judge Jev's uncertainty, and there is none to judge here.
            if (paging := _paging(state, observation)) is not None:
                if await self._step(state, observation, paging, Decider.LLM, gate=False):
                    await self._recover(state, observation, _read_exhausted(state))
                continue
            raw = self._raw_observation or observation
            origin = origin_of(raw.url)
            page = (raw.url, raw.document_key)
            if state.planning.done():
                await state.await_plan()
            secrets = self._secret_names(origin)
            # A held secret makes a sign-in wall work to do rather than a stop, but passes no CAPTCHA, so the
            # bot check is asked on every page a secret covers too.
            fresh = page != state.last_page
            context = self._context(state, secrets, check_login=fresh and not secrets, check_bot=fresh)
            decision = await self._unless_redrawn(
                state, decide(self._jev, observation, context, self._config, ledger=state.ledger), observation, None
            )
            if decision is None:
                continue
            # Only an answered decision has asked the page's sign-in and bot questions; a dropped one asked nothing.
            state.last_page = page
            bot_check = (decision.bot_check or 0.0) > self._config.thresholds.bot_check_above
            if bot_check or (decision.login_required or 0.0) > self._config.thresholds.login_required_above:
                # A wall offering nothing to act on cannot be signed into. It is a bot check such as PyPI's
                # "Client Challenge", which clears itself once its script runs, and stopping on it failed
                # five runs in six of a task hosted agents finish by waiting.
                # A bot check that draws a CAPTCHA has controls, and may still clear itself before it does.
                if (bot_check or not raw.controls) and await self._outwait(raw):
                    continue
                if bot_check:
                    raise _Stop(
                        Status.BLOCKED, f"bot check at {origin}; it is not a sign-in and no credential passes it"
                    )
                raise _Stop(Status.NEEDS_LOGIN, f"sign-in required at {origin}")
            uncertain = decision.confidence < self._config.thresholds.recover_below
            decided_by = Decider.JEV
            if (uncertain or decision.operation not in _NOT_ACTING) and (
                directed := _follow_recovery(state, observation, decision, uncertain=uncertain)
            ) is not None:
                decision, uncertain, decided_by = directed, False, Decider.LLM
            if await self._read_before_interaction(state, observation, decision, decided_by):
                continue
            pager = (
                decision.operation is Operation.CLICK and decision.target is not None and pager_link(decision.target)
            )
            # Only a pager waits for the plan: the clicks that set a search up run while it is still being written.
            plan = await state.await_plan() if pager else state.ready_plan
            if decision.operation is Operation.CLICK and plan is not None:
                # A lookup has nothing left to do once it is answered: with the cheapest flight read, Jev went on to
                # click "Select flight", which the page covered, and the recording showed a failed click after the
                # answer. A click recovery directed stands, so a DONE the verifier refused is not asked again.
                lookup = all(r.kind is RequirementKind.INFORMATION for r in plan.requirements)
                if (pager or (lookup and decided_by is Decider.JEV)) and _answered(plan, state.notes):
                    # Everything asked to be found is evidenced, so another page is wandering: Jev, offered the pager,
                    # kept turning pages through a whole catalogue after the two the task named had been read. DONE
                    # is judged again by `_finish`, which carries on if it does not hold.
                    decision, uncertain, decided_by = _code_decision(Operation.DONE, None), False, Decider.LLM
                elif pager and not state.read_here and _unread(plan, state.notes):
                    # Turning the page of a list nobody has read loses that page: with the pager in view Jev opened
                    # the next page from the first, and a task over "this page and the next" was answered from the
                    # second and third. Read here first, which also tells code whether the list goes on.
                    decision, uncertain = (
                        decision.model_copy(update={"operation": Operation.READ, "target": None}),
                        False,
                    )
            if uncertain and not raw.controls and await self._outwait(raw):
                # Nothing to act on and no idea what to do is a page still rendering: a script-built app settles
                # before it draws, and recovery on it saw an empty login form and spent 5 to 13s saying so.
                continue
            if uncertain and state.ready_plan is None:
                # Unsure without the requirements: the plan is already in flight and costs less than recovery.
                await state.await_plan()
                continue
            if decision.operation in _NOT_ACTING:
                plan = await state.await_plan()
                # Notes that evidence every requirement can still describe the page before the last interaction
                # redrew it, so an answer owed after one is read off what it drew. A plan that only acts has
                # nothing to read, and finishes without waiting.
                if _unread(plan, state.notes) or (state.owes_read and plan.answer_expected):
                    reading = decision.model_copy(update={"operation": Operation.READ, "target": None})
                    # A read that ran spends the direction it was sent on, but may leave one of its own: the control
                    # the reader named to show more was cleared right after the read that named it, and Flights
                    # runs went to recovery without clicking View more flights. A skipped read spends nothing.
                    held, state.directed = state.directed, None
                    if not await self._step(state, observation, reading, decided_by):
                        continue
                    state.directed = held
                    if not _unread(plan, state.notes):
                        # Only an owed read gets here: a scroll changes the page but not its text, so the read
                        # found content already read, and the notes already describe what the interaction drew.
                        decision = decision.model_copy(update={"operation": Operation.DONE, "target": None})
                    else:
                        directed = (
                            decision
                            if decision.directed
                            else _follow_recovery(state, observation, decision, uncertain=True)
                        )
                        if directed is None:
                            await self._recover(state, observation, _read_exhausted(state))
                            continue
                        decision, uncertain, decided_by = directed, False, Decider.LLM
                else:
                    state.directed = None
                    decision = decision.model_copy(update={"operation": Operation.DONE, "target": None})
            # The confidence gate exists to stop the agent acting on a page it does not understand. READ and DONE
            # do not act: a read changes nothing, and DONE is judged again by `_finish`. Jev splitting DONE from
            # READ on the page that shows the answer sent every such run to recovery, and one spent the whole
            # recovery budget there and ended without an answer.
            if decision.operation is Operation.ESCALATE or (
                uncertain and decision.operation not in _NOT_ACTING and not _try_unsure(state, observation)
            ):
                await self._recover(state, observation, f"uncertain next step ({decision.confidence:.2f})")
                continue
            if decision.operation is Operation.DONE:
                result = await self._finish(state, observation, output_schema, until)
                if result is not None:
                    return result
                continue
            attempted = state.attempts.get(_signature(decision, observation))
            if attempted is not None and attempted.idle:
                # The same click from the same page already did nothing; taking it again is the loop #12 names,
                # Done and Search clicked three times each on a form that would not submit. Recovery is told so once:
                # a click can also do nothing because the page had not wired it up yet, and a retry it asks for stands.
                attempted.idle = False
                named = _describe(decision.target) if decision.target else decision.tab_id
                reason = f"{decision.operation.value} {named or ''} already did nothing here".strip()
                await self._recover(state, observation, reason)
                continue
            try:
                if await self._step(state, observation, decision, decided_by):
                    await self._recover(state, observation, _read_exhausted(state))
            except _Unsure as unsure:
                await self._recover(state, observation, str(unsure), gives_up_as=unsure.gives_up_as)

    async def _unless_redrawn[T](
        self, state: _RunState, work: Coroutine[None, None, T], observation: Observation, target: Control | None
    ) -> T | None:
        """`work`'s result, or None when the page redrew under it: `target`, or with none any control offered.

        A page can draw after it settles, and nothing says it will: picking a day in Google Flights' date picker
        blanks the prices, the page waits about 400ms with no request, spinner or mutation, and then fetches and
        draws them. Those prices are the text around the picker's Done button, which authorizes the click, so a
        Done chosen and gated on the blank picker was refused as stale in nearly every run and decided again.
        Watching while Jev decides and gates drops the work as soon as it is doomed, and the next decision is made
        on the settled page. Only one drop per action: a control that never stops changing is still acted on.
        """
        if state.redecided:
            return await work
        working = asyncio.create_task(work)
        watching = asyncio.create_task(
            self._page.redrawn(
                self._raw_observation or observation, _REDRAW_WATCH_SECONDS, target_id=target.id if target else None
            )
        )
        try:
            await asyncio.wait({working, watching}, return_when=asyncio.FIRST_COMPLETED)
            if not working.done() and watching.result():
                trace("redecide", url=self._redactor.redact_url(observation.url))
                state.redecided = True
                return None
            return await working
        finally:
            await _discard(watching)
            await _discard(working)

    async def _first_page(self, task: str, ledger: Ledger) -> str:
        """The page to begin on when the caller named none.

        An embedder whose own interface takes a goal and no URL (an agent handing over a sentence) has
        nowhere to get one, and a browser opened on a blank page gives Jev nothing to choose between. So the
        address is proposed from the task, exactly as the shortcut is, and the run begins there.
        """
        proposal = await propose_start(self._llm, task, ledger=ledger)
        ledger.record(proposal.cost)
        opening = accept_start(proposal.data.url)
        if opening is None:
            raise _Stop(Status.NEEDS_INPUT, "the task names no page to start from, and none was given")
        trace("start_page", url=opening)
        return opening

    async def _front_page_if_blank(self, opened: str) -> None:
        """Go to the site's front page when the address this run chose for itself opened nothing.

        A start page worked out from the task is a guess at an address, and a guess can name a path the site
        does not serve - `/login` on a site that only serves `/`. The run then reads a page with nothing on
        it, learns nothing, and spends its recovery discovering it is stuck: three empty reads and a
        navigation, in the run that prompted this. Only the path was ever in doubt, so the front page of the
        site already chosen is where to be. A start page the caller gave is left alone: that address is not
        a guess and this run is not entitled to second-guess it.
        """
        proposed = urlsplit(opened)
        if proposed.path in ("", "/"):
            return
        observation = await self._observe()
        if _drew_something(observation):
            return
        # A page built by script is observable before it draws: navigation returns at `readyState`, and
        # hydration follows. Waiting is what the loop already does before calling an empty page stuck, and
        # without it a deep route that was right would be abandoned for being slow.
        if await self._outwait(observation) and _drew_something(await self._observe()):
            return
        front = f"{proposed.scheme}://{proposed.netloc}"
        trace("start_page_blank", proposed=opened, front=front)
        await self._page.navigate(front)

    async def _open(self, task: str, start: str, ledger: Ledger) -> tuple[list[HistoryEntry], set[str]]:
        """Open `start`, or a direct address for the task on its site when one is proposed in time.

        The proposal is written while the start page loads, so it costs no wall time unless it outlasts the load,
        and the start page stays one BACK away for when the shortcut lands somewhere unhelpful.
        """
        proposing = asyncio.create_task(self._propose(task, start, ledger))
        try:
            await self._page.navigate(start)
            proposal = await asyncio.wait_for(asyncio.shield(proposing), _SHORTCUT_GRACE_SECONDS)
        except (TimeoutError, LLMError):
            return [], set()
        finally:
            await _discard(proposing)
        shortcut = accept(proposal.url, start)
        if shortcut is None:
            return [], set()
        try:
            await self._page.navigate(shortcut)
            # `accept` saw only the proposed address; a redirect can still land on another site.
            landed = await self._page.address()
            status = await self._page.response_status()
        except BrowserError:
            landed, status = None, None
        if landed is None or origin_of(landed) != origin_of(start):
            logger.warning("shortcut %s did not stay on %s; returning to the start page", shortcut, start)
            await self._page.navigate(start)
            return [], set()
        if status is not None and status >= 400:
            # A proposed address is a guess, and a guess can name a path the site does not serve: books-mystery
            # opened `mysteryfile_3/`, read a 404 and spent a BACK leaving it, every run. The start page is known good.
            logger.info("shortcut %s answered HTTP %s; staying on the start page", shortcut, status)
            await self._page.navigate(start)
            return [], set()
        note = f"opened {shortcut} directly instead of clicking there; the start page {start} is one BACK away"
        opened = HistoryEntry(operation=None, target=None, outcome=StepOutcome.EXECUTED, page_changed=True, note=note)
        # The facts cite where the page landed, so a redirect's address is the one they can be matched against.
        return [opened], {shortcut, landed}

    async def _propose(self, task: str, start: str, ledger: Ledger) -> Shortcut:
        # Recorded here, not by the caller: a proposal that finished is billed even when the run ends first.
        generation = await propose_shortcut(self._llm, task, start, ledger=ledger)
        ledger.record(generation.cost)
        return generation.data

    async def _outwait(self, stuck: Observation) -> bool:
        """Re-observe until the page is no longer `stuck`, returning whether it moved in time."""
        deadline = time.monotonic() + _INTERSTITIAL_SECONDS
        while time.monotonic() < deadline:
            await asyncio.sleep(_INTERSTITIAL_POLL_SECONDS)
            if (await self._observe()).page_key != stuck.page_key:
                return True
        return False

    async def _step(
        self,
        state: _RunState,
        observation: Observation,
        decision: Decision,
        decided_by: Decider = Decider.JEV,
        *,
        capture: Capture | None = None,
        gate: bool = True,
    ) -> bool:
        """Return whether a duplicate read was skipped, so callers can recover or continue the interaction."""
        started = time.monotonic()
        facts_before = len(state.notes.facts)
        reason = state.hint if decision.directed else None
        label = _describe(decision.target) if decision.target else decision.tab_id
        typed: str | None = None
        effect_now: str | None = None
        if decision.operation is Operation.READ:
            progressed, skipped = await self._read(state, capture or await self._capture(), observation)
            state.read_here = True
            if skipped:
                return True
            changed = False
            effect_now = "Read this content."
            if _unread(state.plan, state.notes):
                effect_now += f" {_read_exhausted(state)}"
            act = ActResult(outcome=StepOutcome.EXECUTED, page_changed=False, detail=effect_now)
        else:
            action = await self._unless_redrawn(
                state, self._action(state, observation, decision, gate=gate), observation, decision.target
            )
            if action is None:
                return False
            state.redecided = False
            if action.secret:
                # Held from the keystrokes on: a page may mirror the value, and only the next reading shows it.
                self._page.withhold_frames(True)
            act = await self._page.act(action, self._raw_observation or observation)
            if act.outcome is StepOutcome.STALE and decision.target is not None:
                act = await self._act_on_twin(action, observation, decision.target) or act
            if act.outcome is StepOutcome.EXECUTED and action.text is not None:
                typed = "<secret>" if action.secret else self._redactor.mask(action.text)
            changed = act.page_changed
            state.owes_read = state.owes_read or (act.outcome is StepOutcome.EXECUTED and changed)
            # A value edit answers "was this progress" itself, and its answer beats `changed`: the popup a fill
            # draws IS a page change, so `changed` alone kept crediting the identical re-fill even once the
            # written-value check had stopped doing so. `changed` decides every other operation.
            written = state.written.setdefault(observation.document_key, set())
            edit = self._edit_progress(decision, action, written)
            if act.outcome is StepOutcome.EXECUTED and edit is not None and decision.target is not None:
                written.add(decision.target.id)
            progressed = act.outcome is StepOutcome.EXECUTED and (changed if edit is None else edit)
            # Moving between two pages changes the page every time, and a run went round "open the author,
            # back to the list" to its step limit with its stall budget reset at every hop. The same action
            # from the same page a third time is going round, not forward.
            signature = _signature(decision, observation)
            attempt = state.attempts.setdefault(signature, _Attempts())
            attempt.count += 1
            if decision.operation is not Operation.SCROLL and attempt.count > _REPEATS_BEFORE_CYCLE:
                progressed = False
            state.acted_from = observation
            target = decision.target
            effective = changed
            if act.outcome is StepOutcome.EXECUTED and target is not None and target.role in SETTING_ROLES:
                # Choosing an option has an intended effect to check: a menu that closed without the value
                # changing still changes the page, and Google Flights' "One way" was clicked to the step limit.
                landed = await self._observe()
                done = effect(observation, landed, target)
                state.pending_move = move(observation, landed)
                state.acted_from = None
                effective = done.set_something
                if not done.set_something:
                    progressed = False
                    act = act.model_copy(update={"detail": f"no effect: {done.summary}"})
                effect_now = done.summary
            # A covered flight control never dispatched, so it escaped idle memory and was picked again.
            attempt.idle = (
                act.outcome in {StepOutcome.EXECUTED, StepOutcome.COVERED}
                and not effective
                and decision.operation in _IDLE_CHECKED
            )
        if changed:
            state.read_here = False
        state.history.append(
            HistoryEntry(
                operation=decision.operation,
                target=label,
                outcome=act.outcome,
                page_changed=changed,
                text=typed,
                effect=effect_now,
            )
        )
        step = StepResult(
            index=len(state.steps),
            operation=decision.operation,
            decided_by=decided_by,
            outcome=act.outcome,
            url=observation.url,
            target=label,
            confidence=decision.confidence if decided_by is Decider.JEV else None,
            note=self._redactor.redact("\n".join(part for part in (reason, act.detail) if part)) or None,
            facts=tuple(self._public_fact(fact) for fact in state.notes.facts[facts_before:]),
            page_changed=None if decision.operation is Operation.READ else changed,
            duration_ms=int((time.monotonic() - started) * 1000),
        )
        await self._record_step(state, step)
        if progressed and changed:
            # Whether a change moved the run forward depends on where it led, which the next observation shows.
            state.left = state_key(observation)
        elif progressed:
            state.unchanged = 0
            state.hint = None
        else:
            state.unchanged += 1
        # Progress breaks the stagnation streak even when setup work has not evidenced a requirement yet.
        if progressed:
            state.plan_marks.clear()
        else:
            state.plan_marks.append(self._plan_mark(state))
        for tripped in self._tripwires(state):
            if tripped.tripwire is Tripwire.NO_PROGRESS or self._config.stall.tripwires is TripwireMode.ARMED:
                await self._recover(state, observation, str(tripped))
                return False
            state.would_fire.append(tripped.tripwire)
            logger.info(
                "tripwire %s would have recovered on step %d",
                tripped,
                len(state.steps),
                extra={"tripwire": tripped.tripwire.value},
            )
        return False

    async def _act_on_twin(self, action: Action, observation: Observation, target: Control) -> ActResult | None:
        """Act on the one control now standing where `target` stood, if the page redrew it since it was observed.

        A date picker redraws its days and its Done button as it animates, so a click chosen a moment earlier finds
        its element gone, and deciding again costs a full model call to pick the same control. A control that is
        the only one retaining its guard and semantics in the same document can inherit that decision. A new
        document at the same address, or a changed form, has not passed the original authorization gate.
        """
        fresh = await self._observe()
        if (
            not observation.document_key
            or not target.retarget_key
            or fresh.document_key != observation.document_key
            or fresh.url != observation.url
            or fresh.title != observation.title
        ):
            return None
        twins = [
            control
            for control in fresh.controls
            if control.model_copy(update={"id": target.id, "offscreen": target.offscreen}) == target
        ]
        if len(twins) != 1:
            return None
        trace("retarget", target=self._redactor.redact(target.label))
        return await self._page.act(
            action.model_copy(update={"target_id": twins[0].id}), self._raw_observation or fresh
        )

    def _settle(
        self, state: _RunState, observation: Observation, *, renews: bool = True, put_back: bool = False
    ) -> str | None:
        """Judge the last page-changing action by the state it led to; the reason to recover, if the run is stuck.

        Recoveries are spent on being stuck, not on the whole run, so a page state never seen before restores the
        budget; any change did before, and a run going round four pages recovered without end. A change is not
        progress either when it leads back to a state the run was already in: on a form Google would not submit,
        Done closed a date picker, Search opened it again, and Done and Search went round for a minute, each click
        a change, until recovery happened to fix the form.
        """
        key = state_key(observation)
        first = state.reached.get(key)
        if first is None:
            state.reached[key] = len(state.history)
            if renews:
                state.recoveries = 0
                state.recovery_log.clear()
        left, state.left = state.left, None
        if left is None:
            return None
        cycle = state.history[first:] if first is not None else []
        # Going back to a list after reading one of its pages is how a comparison is done, not a wasted round.
        if first is None or key == left or any(entry.operation is Operation.READ for entry in cycle):
            if put_back:
                # A setting put back to a state its page already held is not progress, however the results
                # redraw beneath it. Without this a filter toggled on and off reached a page state never seen
                # on every click, so nothing counted it and the run toggled until the step limit.
                state.unchanged += 1
                return None
            state.unchanged = 0
            state.hint = None
            return None
        undone = ", ".join(_described(entry) for entry in cycle[-_CYCLE_SHOWN:])
        note = (
            f"back to a page state first reached {len(cycle)} actions ago; "
            f"the actions since ({undone}) undid each other"
        )
        last = state.history[-1]
        state.history[-1] = last.model_copy(update={"effect": f"{last.effect}; {note}" if last.effect else note})
        # One return is already a loop: waiting for the stall count let Search and Done go round three times, with
        # an unsure step's recovery in between sending the run off to re-fill the origin.
        return note

    @staticmethod
    def _reversal(state: _RunState) -> tuple[str | None, bool, bool]:
        """The reason to recover when a setting went back to an earlier value, and whether the state reached may
        renew the recovery budget. Settings put back to values their page already held are not progress, however
        the results redraw: a filter toggled on and off reached a state never seen on every click."""
        made, state.pending_move = state.pending_move, None
        at = len(state.history)
        state.moves = [(index, m) for index, m in state.moves if index >= at - _REVERSAL_WINDOW]
        if (
            made is None
            or made.values_after == made.values_before
            or state.history[-1].outcome is not StepOutcome.EXECUTED
            # Scrolling shows and hides controls without changing a setting.
            or state.history[-1].operation in _PAGE_OPERATIONS
        ):
            return None, True, False
        held = made.document, frozenset(made.values_after.items())
        renews = held not in state.settings_held
        # Nothing committed means nothing was put anywhere, so there is no earlier state to have returned to.
        put_back = bool(made.values_after) and not renews
        state.settings_held.update((held, (made.document, frozenset(made.values_before.items()))))
        note = None
        for index, earlier in reversed(state.moves):
            # Recovery spent what came before it, and a read in between makes a return a comparison, as in _settle.
            if index <= state.recovered_at or any(e.operation is Operation.READ for e in state.history[index:]):
                break
            if (returned := reversal(made, earlier)) is not None:
                actions = ", ".join(_described(entry) for entry in state.history[index - 1 :][-_CYCLE_SHOWN:])
                note = f"{returned}; intervening actions: {actions}"
                break
        state.moves.append((at, made))
        return note, renews, put_back

    @staticmethod
    def _note_effect(state: _RunState, observation: Observation) -> None:
        """Record on the last action what it did, which the next choice and recovery both read."""
        before, state.acted_from = state.acted_from, None
        if before is None or not state.history or state.history[-1].effect is not None:
            return
        state.history[-1] = state.history[-1].model_copy(update={"effect": effect(before, observation).summary})
        state.pending_move = move(before, observation)

    async def _observe(self) -> Observation:
        """Every observation models see has resolved secret values blanked, wherever the page echoes them."""
        observation = await self._page.observe()
        self._raw_observation = observation
        self._secret_on_screen = self._reveals(observation)
        mask = self._redactor.mask
        controls = tuple(
            control.model_copy(
                update={
                    "label": mask(control.label),
                    "context": None if control.context is None else mask(control.context),
                    "value": None if control.value is None else mask(control.value),
                    "href": None if control.href is None else mask(control.href),
                    "frame_origin": None if control.frame_origin is None else mask(control.frame_origin),
                    "submit_semantics": None if control.submit_semantics is None else mask(control.submit_semantics),
                    "options": tuple(mask(option) for option in control.options),
                }
            )
            for control in observation.controls
        )
        return observation.model_copy(
            update={
                "url": mask(observation.url),
                "title": mask(observation.title),
                "viewport_text": mask(observation.viewport_text),
                "controls": controls,
                "tabs": tuple(
                    t.model_copy(update={"url": mask(t.url), "title": mask(t.title)}) for t in observation.tabs
                ),
                "dialog": observation.dialog.model_copy(
                    update={
                        "message": mask(observation.dialog.message),
                        "default_prompt": mask(observation.dialog.default_prompt)
                        if observation.dialog.default_prompt is not None
                        else None,
                    }
                )
                if observation.dialog
                else None,
            }
        )

    async def _capture(self) -> Capture:
        capture = await self._page.capture()
        text = self._redactor.mask(capture.text)
        digest = hashlib.sha256(text.encode()).hexdigest()
        mask = self._redactor.mask
        return capture.model_copy(
            update={
                "text": text,
                "url": mask(capture.url),
                "title": mask(capture.title),
                "sha256": digest,
                "blocks": tuple(
                    block.model_copy(
                        update={
                            "heading_path": tuple(mask(heading) for heading in block.heading_path),
                            "href": None if block.href is None else mask(block.href),
                        }
                    )
                    for block in capture.blocks
                ),
            }
        )

    async def _screenshots(self) -> tuple[bytes, ...]:
        """No image while a secret shows as page text: pixels cannot be masked like text. Typed fields are masked
        by the page itself."""
        return () if self._secret_on_screen else (await self._page.screenshot(),)

    def _secret_names(self, origin: str) -> tuple[str, ...]:
        """Stored secrets this origin may receive. With any, a sign-in wall is a step to take, not a stop."""
        if self._secrets is None:
            return ()
        return tuple(ref.name for ref in self._secrets.available() if secret_allowed(ref, origin))

    @staticmethod
    def _edit_progress(decision: Decision, action: Action, written: Set[str]) -> bool | None:
        """Rewriting the observed field value vetoes progress even when the page changes, and the first new value
        a field receives is progress even when the page does not: a page's fingerprint ignores field values, so
        a three-field checkout form counted three unchanged steps and tripped the stall recovery every run.
        A later rewrite of a field already written returns None, as do other edits, including uploads, so the
        page's effect decides; crediting every new value let a run grind on a search box it never submitted.
        """
        if decision.operation not in {Operation.FILL, Operation.SELECT} or decision.target is None:
            return None
        if action.text is None:
            return None
        # A secret's value never leaves the page; only its length is observed, so that is what to compare.
        held = "\u2022" * len(action.text) if action.secret else action.text
        if decision.target.value == held:
            return False
        return True if decision.target.id not in written else None

    async def _record_step(self, state: _RunState, step: StepResult) -> None:
        state.steps.append(step)
        state.ledger.steps += 1
        if self._on_event is None:
            return
        # Taken from the page the step acted on, before the next observation moves it on.
        await self._on_event(StepEvent(step=step, frame=await self._frame() if self._config.step_frames else None))

    async def _record_failure(
        self,
        state: _RunState,
        observation: Observation,
        operation: Operation,
        reason: str,
        *,
        target: str | None = None,
        confidence: float | None = None,
        page_changed: bool | None = None,
        decided_by: Decider,
    ) -> None:
        await self._record_step(
            state,
            StepResult(
                index=len(state.steps),
                operation=operation,
                decided_by=decided_by,
                outcome=StepOutcome.FAILED,
                url=observation.url,
                target=target,
                confidence=confidence,
                note=reason,
                page_changed=page_changed,
                duration_ms=0,
            ),
        )

    async def _frame(self) -> bytes | None:
        """A PNG of the page as it is now, or None while a resolved secret is showing on it.

        The check is made against a fresh reading of the page rather than `_secret_on_screen`, which was
        computed by the observation this step was decided from: that is the page BEFORE the action ran, and the
        action may be the one that put the secret there. A field a page mirrors into ordinary text would
        otherwise reach the caller as pixels, which is the one thing a frame must never carry.
        """
        if self._reveals(await self._page.observe()):
            return None
        return await self._page.screenshot()

    def _reveals(self, observation: Observation) -> bool:
        """Whether a resolved secret shows on the page; live and recorded frames are held back while one does."""
        revealed = self._redactor.reveals(observation.model_dump_json())
        self._page.withhold_frames(revealed)
        return revealed

    async def _action(self, state: _RunState, observation: Observation, decision: Decision, *, gate: bool) -> Action:
        target = decision.target
        match decision.operation:
            case Operation.CLICK | Operation.ENTER:
                # Only a pager link to another address (`next_page_control`) goes ungated: it opens a page and
                # commits nothing, so asking Jev would buy a call per page and nothing else.
                if gate:
                    await self._gate_irreversible(state, observation, decision)
                return Action(operation=decision.operation, target_id=target.id if target else None)
            case Operation.FILL:
                text = await self._text(state, observation, _require(target))
                return Action(
                    operation=Operation.FILL,
                    target_id=_require(target).id,
                    text=text,
                    secret=self._redactor.reveals(text),
                    secret_origin=self._target_origin(_require(target)),
                )
            case Operation.SELECT:
                raw = self._raw_target(_require(target))
                option = await self._choose(
                    state,
                    observation,
                    f"Which option should {_require(target).label!r} be set to?",
                    raw.options,
                )
                return Action(operation=Operation.SELECT, target_id=raw.id, text=option)
            case Operation.UPLOAD:
                if not state.attachments:
                    raise _Stop(Status.NEEDS_INPUT, "the page asks for a file and none was provided")
                name = await self._choose(
                    state, observation, "Which file belongs in this input?", tuple(a.name for a in state.attachments)
                )
                files = tuple(a for a in state.attachments if a.name == name)
                if sum(len(f.content) for f in files) > self._config.max_upload_bytes:
                    raise _Stop(Status.NEEDS_INPUT, f"{name} exceeds the upload size limit")
                return Action(operation=Operation.UPLOAD, target_id=_require(target).id, files=files)
            case Operation.DIALOG:
                accept = await self._accept_dialog(state, observation)
                if accept and observation.dialog and observation.dialog.kind in {"confirm", "prompt", "beforeunload"}:
                    await self._gate_question(
                        state,
                        observation,
                        decision,
                        observation.dialog.message,
                        NoulQuestion(
                            instructions=(
                                f"{UNTRUSTED}\nTask: {state.task}\nThe agent is about to accept this "
                                f"{observation.dialog.kind} dialog: {observation.dialog.message!r}. Would accepting "
                                "commit an irreversible or externally visible change, such as deleting data, sending a "
                                "message or spending money?"
                            ),
                            true="Acceptance commits a destructive or externally visible change.",
                            false="Acceptance only navigates, reveals information or edits a reversible draft.",
                        ),
                    )
                return Action(operation=Operation.DIALOG, accept_dialog=accept)
            case Operation.SWITCH_TAB:
                return Action(operation=Operation.SWITCH_TAB, tab_id=decision.tab_id)
            case Operation.HOVER:
                return Action(operation=Operation.HOVER, target_id=_require(target).id)
            case Operation.ESCAPE | Operation.SCROLL | Operation.BACK:
                return Action(operation=decision.operation)
            case Operation.READ | Operation.DONE | Operation.ESCALATE:
                raise ValueError(f"{decision.operation} is handled by the loop, not dispatched")

    async def _gate_irreversible(self, state: _RunState, observation: Observation, decision: Decision) -> None:
        target = decision.target
        if target is None or not may_be_irreversible(decision.operation, target):
            return
        await self._gate_question(
            state,
            observation,
            decision,
            _describe(target),
            irreversible_question(state.task, decision.operation, target),
        )

    async def _gate_question(
        self, state: _RunState, observation: Observation, decision: Decision, label: str, question: NoulQuestion
    ) -> None:
        thresholds = self._config.thresholds
        authorized = state.authorization.irreversible_actions
        # Authorized and confident proceeds whatever Jev would say about the action, so it is not asked. Only Jev's
        # own confidence counts: a directed action carries the score of the action Jev chose instead, and a
        # confident READ would otherwise wave recovery's click through unasked.
        if authorized and not decision.directed and decision.confidence >= thresholds.sensitive_act_from:
            return
        state.ledger.reserve(CostComponent.JEV)
        # Only the address goes with the question: the page's own text is what would argue a harmful action
        # is harmless, and the control's label and form are already in the question.
        evaluation = await self._jev.evaluate(
            {"page": {"url": observation.url, "title": observation.title}}, {"irreversible": question}
        )
        state.ledger.record(evaluation.cost)
        answer = evaluation.answers.get("irreversible")
        if isinstance(answer, NoulAnswer) and answer.probability <= thresholds.irreversible_above:
            return
        what = f"{decision.operation.value} {label!r}"
        # An unsure pick that may commit something is more likely the wrong pick than the step to confirm.
        unsure = authorized or decision.confidence < thresholds.recover_below
        reason = (
            f"unsure {what} is the irreversible action the task means ({decision.confidence:.2f})"
            if unsure
            else f"{what} needs confirmation"
        )
        await self._record_failure(
            state,
            observation,
            decision.operation,
            self._redactor.redact(reason),
            target=self._redactor.redact(label),
            confidence=None if decision.directed else decision.confidence,
            page_changed=False,
            decided_by=Decider.LLM if decision.directed else Decider.JEV,
        )
        if unsure:
            raise _Unsure(reason)
        raise _Stop(Status.NEEDS_CONFIRMATION, reason)

    async def _text(self, state: _RunState, observation: Observation, target: Control) -> str:
        secrets = tuple(ref.name for ref in self._secrets.available()) if self._secrets else ()
        origin = self._target_origin(target)
        if target.sensitive:
            return await self._sensitive_text(state, observation, target, secrets, origin)
        criteria: dict[str, JsonValue] = {
            f"input:{k}": f"The provided value named {k}: {v}" for k, v in state.inputs.items()
        }
        criteria |= {f"secret:{name}": f"The stored secret named {name} (value hidden)" for name in secrets}
        criteria[GENERATE] = "None of these; write new text stated in the task or notes."
        # Amazon labels its sign-in field "Enter mobile number or email", and Jev, seeing only a secret named
        # `username`, chose to write new text; the task gives no email, so the run stopped needs_input.
        # Only for signing in: a checkout's contact email is the task's value, not the login.
        question = (
            f"What should be typed into {target.label!r}? Stored secrets are this site's sign-in credentials, "
            "named by role, and are only for signing in to the account: when this field is a sign-in field, the "
            "username is also the account's email address or phone number. Any other field takes a value the task "
            "gives."
        )
        choice = await self._ask_choice(state, observation, question, criteria) if len(criteria) > 1 else GENERATE
        if choice.startswith("input:"):
            return state.inputs[choice.removeprefix("input:")]
        if choice.startswith("secret:"):
            return await self._secret(choice.removeprefix("secret:"), origin)
        return await self._generate_text(state, observation, target)

    async def _sensitive_text(
        self, state: _RunState, observation: Observation, target: Control, secrets: tuple[str, ...], origin: str
    ) -> str:
        """A password field takes a stored secret or nothing: a generated value is at best a guess, and a guess
        that happens to work (a demo site's well-known password) is a pass nobody authorized."""
        match secrets:
            case ():
                raise _Stop(Status.NEEDS_LOGIN, f"{target.label!r} wants a secret and none is stored")
            case (only,):
                return await self._secret(only, origin)
            case _:
                criteria: dict[str, JsonValue] = {name: f"The stored secret named {name}" for name in secrets}
                question = f"Which stored secret belongs in {target.label!r}?"
                return await self._secret(await self._ask_choice(state, observation, question, criteria), origin)

    def _raw_target(self, target: Control) -> Control:
        if self._raw_observation is not None:
            return next(c for c in self._raw_observation.controls if c.id == target.id)
        return target

    def _target_origin(self, target: Control) -> str:
        return self._raw_target(target).frame_origin or "null"

    async def _secret(self, name: str, origin: str) -> str:
        value = await resolve_secret(self._secrets, name, origin) if self._secrets else None
        if value is None:
            raise _Stop(Status.NEEDS_INPUT, f"secret {name} is not available for {origin}")
        self._redactor.register(name, value, origin)
        return value

    async def _generate_text(self, state: _RunState, observation: Observation, target: Control) -> str:
        # Adapted from browser-use/jev-ultrafast (MIT), model.py:field_context. A popup's field
        # can have a generic label; the opening action and surrounding values explain its purpose.
        context = {
            "task": state.task,
            "subgoal": state.hint,
            "field": target.model_dump(mode="json", exclude_none=True),
            "other_fields": [
                control.model_dump(
                    mode="json", include={"label", "context", "role", "value", "input_type"}, exclude_none=True
                )
                for control in observation.controls
                if control.id != target.id and (Operation.FILL in control.operations or control.role == "combobox")
            ],
            "page": {
                "url": observation.url,
                "title": observation.title,
                "text": observation.viewport_text,
                "date": observation.captured_at.date().isoformat(),
            },
            "recent_actions": [
                entry.model_dump(mode="json", exclude_none=True)
                for entry in _history(state.history, self._config.observation)
            ],
            "notes": state.notes.render(self._config.observation.working_notes_chars),
        }
        messages = [
            Message(role="system", content=_FIELD_WRITER),
            Message(role="user", content=json.dumps(context)),
        ]
        written = await self._write_field(state, messages)
        if written is not None:
            return written
        # Ending a run on "you never told me" is right, and one low-effort call is a thin thing to end it on:
        # the writer called a surname the task had given it missing in a third of checkout runs. Jev reads the
        # same task and notes, so it is asked whether the value really is absent before the run stops, and the
        # writer gets one more attempt with the disagreement put to it.
        if await self._value_absent(state, observation, target):
            raise self._missing(state, target)
        insisted = [
            *messages,
            Message(
                role="user",
                content=(
                    "You reported this value as missing, but the task or notes appear to state it, or to state "
                    "something it is part of. Look again and write it. Report it missing only if it truly is "
                    "not there: never invent one."
                ),
            ),
        ]
        written = await self._write_field(state, insisted)
        if written is None:
            raise self._missing(state, target)
        return written

    @staticmethod
    def _missing(state: _RunState, target: Control) -> Exception:
        """A field the task gives no value for is first a wrong pick, then a missing input.

        Most such fields are optional, and the right move is to leave them: Google Flights opens a "Where else?"
        box beside the origin, and a run stopped there at needs_input with nothing yet searched. Recovery is told
        once, so it can pick another step; a required field it sends the run back to still ends it here."""
        key = (target.label, target.context)
        if key in state.missing:
            return _Stop(Status.NEEDS_INPUT, f"{target.label!r} needs a value the task does not give")
        state.missing.add(key)
        return _Unsure(
            f"the task gives no value for {target.label!r}: leave it unless the task cannot go on without it",
            gives_up_as=Status.NEEDS_INPUT,
        )

    async def _write_field(self, state: _RunState, messages: Sequence[Message]) -> str | None:
        """The text for one field, or None when the writer says the value was never given."""
        generation = await self._llm.generate(LLMPurpose.FIELD_TEXT, list(messages), _FieldText, ledger=state.ledger)
        state.ledger.record(generation.cost)
        return None if generation.data.missing else generation.data.text

    async def _value_absent(self, state: _RunState, observation: Observation, target: Control) -> bool:
        state.ledger.reserve(CostComponent.JEV)
        question = NoulQuestion(
            instructions=(
                f"{UNTRUSTED}\n\n# Task\n{state.task}\n\nThe agent must fill the field labelled {target.label!r}. "
                "Do the task or the notes give what belongs in it?"
            ),
            true="The task or the notes state that value, or state something it is a part of.",
            false="Neither the task nor the notes say it; filling the field would mean inventing one.",
        )
        evaluation = await self._jev.evaluate(
            page_state(observation, state.notes, self._config.tokens, questions=[question.model_dump_json()]),
            {"stated": question},
        )
        state.ledger.record(evaluation.cost)
        answer = evaluation.answers.get("stated")
        stated = answer.probability if isinstance(answer, NoulAnswer) else 0.0
        return stated <= self._config.thresholds.value_stated_above

    async def _choose(self, state: _RunState, observation: Observation, question: str, options: Sequence[str]) -> str:
        if len(options) == 1:
            return options[0]
        if not options:
            raise _Stop(Status.STUCK, f"no options to answer: {question}")
        keys = {str(i): option for i, option in enumerate(options[: self._config.observation.max_choice_options])}
        criteria: dict[str, JsonValue] = {key: self._redactor.mask(option) for key, option in keys.items()}
        return keys[await self._ask_choice(state, observation, question, criteria)]

    async def _ask_choice(
        self, state: _RunState, observation: Observation, question: str, criteria: Mapping[str, JsonValue]
    ) -> str:
        state.ledger.reserve(CostComponent.JEV)
        choice = ChoiceQuestion(instructions=f"{UNTRUSTED}\n\n# Task\n{state.task}\n\n{question}", criteria=criteria)
        evaluation = await self._jev.evaluate(
            page_state(observation, state.notes, self._config.tokens, questions=[choice.model_dump_json()]),
            {"pick": choice},
        )
        state.ledger.record(evaluation.cost)
        answer = evaluation.answers["pick"]
        if not isinstance(answer, ChoiceAnswer):
            raise JevError("expected a choice answer")
        return answer.choice

    async def _accept_dialog(self, state: _RunState, observation: Observation) -> bool:
        dialog = observation.dialog
        if dialog is None:
            raise _Stop(Status.STUCK, "DIALOG chosen with no dialog open")
        choice = await self._ask_choice(
            state,
            observation,
            (
                f"The page shows a {dialog.kind} dialog saying {dialog.message!r}, opened by the agent's last action. "
                "Accept it if it asks to go ahead with what the task wants done; dismiss it if it would do "
                "something the task did not ask for."
            ),
            {"accept": "Accept / OK", "dismiss": "Dismiss / Cancel"},
        )
        return choice == "accept"

    async def _read_before_interaction(
        self, state: _RunState, observation: Observation, decision: Decision, decided_by: Decider = Decider.JEV
    ) -> bool:
        # A read takes in the whole page, so a scroll over one never read only spends steps: Jev judges evidence from
        # the viewport, and scrolled a country list for Mongolia until recovery ran out and the run stopped stuck.
        unread_scroll = decision.operation is Operation.SCROLL and not any(
            key[0] == observation.document_key for key in state.reads
        )
        if decision.operation in _NOT_ACTING or (
            decision.read_assessment is not ReadAssessment.EVIDENCE and not unread_scroll
        ):
            return False
        plan = await state.await_plan()
        if not _unread(plan, state.notes):
            return False
        # An interaction can remove evidence, so read first and reconsider before authorizing the next action.
        reading = decision.model_copy(update={"operation": Operation.READ, "target": None})
        return not await self._step(state, observation, reading, decided_by)

    async def _read(
        self, state: _RunState, capture: Capture | None = None, observation: Observation | None = None
    ) -> tuple[bool, bool]:
        """Return (added evidence, skipped duplicate), since only new evidence makes a read progress."""
        capture = capture or await self._capture()
        plan = await state.await_plan()
        wanted = [r for r in state.notes.unresolved(plan) if r.kind is RequirementKind.INFORMATION]
        owed = not wanted and state.owes_read and plan.answer_expected
        if owed:
            # Every requirement was evidenced off the page before the last interaction redrew it, so they are
            # asked again of what it drew: a reader asked nothing would leave the pre-filter fare answering.
            wanted = [r for r in plan.requirements if r.kind is RequirementKind.INFORMATION]
        budget: ReadKey | None = None
        if observation is not None:
            wanted_ids = tuple(r.id for r in wanted)
            # Keyed by address too: a list paged in place keeps its document and its Previous and Next, and only
            # its URL says the third page is not the two barren ones before it.
            budget = observation.document_key, state_key(observation), wanted_ids
            # An owed read is exempt: a filter that keeps its address and controls shares the budget of the page
            # before it, and a skip there either finished on the pre-filter fare or turned every later DONE into
            # a skipped read and recovery. It is owed at most once per interaction, so it cannot read for ever.
            if not owed and state.barren.get(budget, 0) >= self._config.stall.barren_reads:
                # The page keeps rewriting its own text, so the exact-content key below never matches and the
                # run could read it until the step budget ran out. What it can do here has paid out nothing.
                trace("read_skipped", reason="no_new_facts_from_this_page_state")
                return False, True
            # Past the barren budget this exact content is either read now or was read before, so the notes hold
            # what the last interaction drew. A barren skip read nothing, and leaves the read owed.
            state.owes_read = False
            key = observation.document_key, capture.sha256, wanted_ids
            if key in state.reads:
                trace("read_skipped", reason="unchanged_content_and_requirements")
                return False, True
            state.reads.add(key)

        def spent(progressed: bool) -> None:
            """A read that paid out clears the budget, so a page that starts answering again is readable."""
            if budget is None:
                return
            if progressed:
                state.barren.pop(budget, None)
            else:
                state.barren[budget] = state.barren.get(budget, 0) + 1

        if not capture.text.strip():
            # An empty page may still be rendering; only new content warrants another read.
            if observation is not None and await self._outwait(observation):
                drawn = await self._capture()
                if drawn.text.strip():
                    return await self._read(state, drawn, observation)
            # Nothing on the page can evidence anything, so the reader is not asked.
            trace("read", url=self._redactor.redact_url(capture.url), chars=0, wanted=[r.id for r in wanted])
            spent(False)
            return False, False
        following = next_page_control(observation) if observation is not None else None
        began = state.first_url if following is not None or state.pages else None
        question = read_question(
            state.task, wanted, began_at=None if began is None else self._redactor.redact_url(began)
        )
        notice = next_page_notice(following)
        before = len(state.notes.facts)
        known = {fact.text for fact in state.notes.facts}
        evidenced = {r.id for r in wanted if state.notes.evidenced(r.id)}
        outcome = await read(
            self._llm,
            capture,
            question,
            [r.id for r in wanted],
            state.notes,
            tokens=self._config.tokens,
            ledger=state.ledger,
            jev=self._jev,
            requirements=wanted,
            notice=notice,
            continuing=state.continuing,
        )
        # Payout is what the notes did not already say. A fact is keyed by the capture it was read from, so a
        # page that rewrites a line re-mints the same records as new facts, and counting them read it for ever.
        progressed = any(fact.text not in known for fact in state.notes.facts) or any(
            state.notes.evidenced(r.id) for r in wanted if r.id not in evidenced
        )
        continues = [key for key in outcome.continues if not state.notes.evidenced(key)]
        trace(
            "read",
            url=self._redactor.redact_url(capture.url),
            chars=len(capture.text),
            wanted=[r.id for r in wanted],
            facts_added=len(state.notes.facts) - before,
            rejected_claims=outcome.rejected_claims,
            evidenced=[r.id for r in wanted if state.notes.evidenced(r.id)],
            continues=continues,
            # Records the reader said this page compared that no run of blocks resolved. A comparison carried
            # forward with these missing is incomplete in the notes, which the trace should say out loud.
            uncovered=outcome.uncovered,
        )
        spent(progressed)
        if observation is not None:
            self._follow_pages(state, continues, following, observation, outcome.expands)
        return progressed, False

    def _follow_pages(
        self,
        state: _RunState,
        continues: Sequence[str],
        following: Control | None,
        observation: Observation,
        expands: str | None,
    ) -> None:
        """Arrange for the rest of a list the reader says it needs: the next page by code, the control the
        reader named, or a word to Jev."""
        state.continuing = set(continues)
        if not continues:
            return
        if following is not None and state.pages < self._config.max_pages:
            # Deciding each hop costs a Jev call to pick a link code has already found, on every page of the list.
            state.next_page = True
            return
        # "Otherwise finish" was a dead end: the reader withholds a list it has not seen the end of, so the done check
        # refused every such finish, and Flights runs spent their recoveries finding "View more flights" instead.
        show = "open the control that loads or pages through more of the list"
        # The reader saw which control shows the rest. Matched against the page's own labels rather than
        # trusted, so a label the reader invented directs nothing. The hint names it too, so Jev is still told
        # where the rest is when a confident decision elsewhere drops the direction.
        if expands is not None:
            named = [c for c in observation.controls if c.label == expands and Operation.CLICK in c.operations]
            if len(named) == 1:
                state.directed = (Operation.CLICK, named[0].id)
                show = f'open "{expands}"'
        state.hint = (
            "The reader saw the list this task needs go on past what the page shows, so what was read cannot settle "
            f"it. Show the rest and READ it: {show}, or narrow the list with the page's own filter or sort so the "
            "answer is in view."
        )

    async def _recover(
        self, state: _RunState, observation: Observation, reason: str, *, gives_up_as: Status = Status.STUCK
    ) -> None:
        reason = self._redactor.redact(reason)
        state.recoveries += 1
        # Recovery spends every tripwire's evidence so the same threshold crossing cannot trigger it again.
        state.unchanged = 0
        state.recovered_at = len(state.history)
        state.plan_marks.clear()
        if state.recoveries > self._config.stall.max_recoveries:
            raise _Stop(gives_up_as, reason)
        steps = "\n".join(
            f"- {h.operation.value if h.operation else 'open'} {h.target or ''} -> {h.outcome.value}"
            + (f": {h.effect}" if h.effect else "")
            for h in _history(state.history, self._config.observation)
        )
        # Without the names, a sign-in page reads as a wall the user must pass: a run with a stored login gave up
        # saying no credentials were given.
        stored = self._secret_names(origin_of(observation.url))
        # Without the notes, a run that had read the answer was told to scroll down "to see the remaining
        # books" three times, and stopped stuck with the answer in hand.
        # Without the reader's word that a list goes on, a note holding the best record SO FAR reads as the answer:
        # a run with the cheapest of the first few flights noted was sent to finish, and the done check refused.
        open_requirements = (
            "\n".join(
                f"- {r.text}"
                + (
                    " (the reader saw the list this ranges over go on past what the page shows: load the rest first)"
                    if r.id in state.continuing
                    else ""
                )
                for r in state.notes.unresolved(state.ready_plan)
            )
            if state.ready_plan
            else ""
        )
        secrets = (
            f"\n\n## Stored secrets\n{', '.join(stored)}. Filling a field with one types its hidden value."
            if stored
            else ""
        )
        generation = await self._llm.generate(
            LLMPurpose.RECOVER,
            [
                Message(
                    role="system",
                    content=(
                        "# Recovery\nThe browsing agent is not making progress. Diagnose why from the screenshot "
                        "and history, then give one concrete next subgoal: a single operation, naming the index of "
                        "the observed control it acts on, with no alternatives. A read, scroll, back or escape acts "
                        "on the page and names no control. A read takes in the whole page, so scroll only to reach "
                        "a control or to load more. When the notes already answer every open requirement, the next "
                        "subgoal is to finish. Recovery memory records earlier diagnoses and subgoals; "
                        "use the recent steps to judge whether to try another way. "
                        "Dates are relative to the supplied current date.\n\n"
                        f"# Trust\n{UNTRUSTED}"
                    ),
                ),
                Message(
                    role="user",
                    content=(
                        f"## Controls\n{_controls_text(observation)}\n\n"
                        f"## Page\n{observation.url}\n{observation.viewport_text}{secrets}\n\n"
                        "## Notes read so far\n"
                        f"{state.notes.render(self._config.observation.working_notes_chars) or 'none'}\n\n"
                        f"## Recent steps\n{steps}\n\n"
                        "## Recovery memory\n"
                        f"{_recovery_memory(state, self._config.stall.max_recoveries, self._redactor)}\n\n"
                        f"## Current date\n{observation.captured_at.date().isoformat()}\n\n"
                        f"## Still to find\n{open_requirements or 'nothing'}\n\n"
                        f"## Task\n{state.task}\n\n## Problem\n{reason}"
                    ),
                    images=await self._screenshots(),
                ),
            ],
            _Recovery,
            ledger=state.ledger,
        )
        state.ledger.record(generation.cost)
        if generation.data.give_up:
            # Asked of the model rather than carried from the step that raised it: a missing value can surface
            # recoveries later, after an attempt to go on without it has failed for its absence.
            status = Status.NEEDS_INPUT if generation.data.needs_input else Status.STUCK
            raise _Stop(status, generation.data.diagnosis)
        state.hint = self._redactor.redact(generation.data.next_subgoal)
        diagnosis = self._redactor.redact(generation.data.diagnosis)
        state.recovery_log.append(
            f"- Reason: {_recovery_text(reason)}. Diagnosis: {_recovery_text(diagnosis)}. "
            f"Subgoal: {_recovery_text(state.hint)}."
        )
        trace(
            "recover",
            reason=reason,
            hint=self._redactor.redact(generation.data.next_subgoal),
            operation=generation.data.operation,
            control=generation.data.control,
        )
        chosen, operation = generation.data.control, generation.data.operation
        if operation is not None and operation in _PAGE_OPERATIONS:
            state.directed = (operation, None)
        elif chosen is not None and operation is not None and 0 <= chosen < len(observation.controls):
            state.directed = (operation, observation.controls[chosen].id)
        await self._record_step(
            state,
            StepResult(
                index=len(state.steps),
                operation=Operation.ESCALATE,
                decided_by=Decider.LLM,
                outcome=StepOutcome.EXECUTED,
                url=observation.url,
                target=None,
                confidence=None,
                note=self._redactor.redact(
                    "\n".join((reason, generation.data.diagnosis, generation.data.next_subgoal))
                ),
                duration_ms=0,
            ),
        )

    async def _finish(
        self,
        state: _RunState,
        observation: Observation,
        output_schema: type[BaseModel] | None,
        until: UntilCheck | None,
    ) -> RunResult | None:
        """Return the final result when DONE holds up; None sends the loop back to work."""
        fresh = await self._observe()
        state.ledger.reserve(CostComponent.JEV)
        await state.await_plan()
        draft = draft_answer(state.plan, state.notes) if state.plan.answer_expected else None
        check = await check_done(
            self._jev,
            state.task,
            state.plan,
            fresh,
            state.notes,
            self._config.thresholds,
            draft,
            tokens=self._config.tokens,
        )
        trace(
            "done_check",
            verdict=check.verdict.value,
            complete=round(check.complete, 3),
            unmet=list(check.unmet),
            requirements={r.id: r.text for r in state.plan.requirements},
        )
        state.ledger.record(check.cost)
        if check.verdict is DoneVerdict.ACCEPT and _guessed(state.plan, state.notes, state.invented):
            # Jev judges the notes, not where they were read, so only the verifier is shown the guessed addresses.
            check = check.model_copy(update={"verdict": DoneVerdict.VERIFY})
        accepted = check.verdict is DoneVerdict.ACCEPT
        missing: tuple[str, ...] = ()
        misread: set[str] = set()
        drafting: asyncio.Task[Generation[ComposedAnswer]] | None = None
        # Whoever still holds the draft when this returns is responsible for it. `finally` covers the
        # paths that are not a decision at all: the verifier raising, `until` raising, the caller
        # cancelling the run. An orphaned compose would otherwise keep calling a model and billing a
        # ledger for a run that has already produced its result.
        try:
            if check.verdict is DoneVerdict.VERIFY:
                # Write the answer while the verifier is still deciding. Both read the same finished
                # notes, and every accepted run wants an answer, so the whole cost of guessing wrong is
                # one discarded call on the branch that was going back to work anyway.
                if state.plan.answer_expected and check.answer is None:
                    drafting = asyncio.create_task(
                        compose(
                            self._llm,
                            state.task,
                            state.plan,
                            state.notes,
                            tokens=self._config.tokens,
                            ledger=state.ledger,
                        )
                    )
                verdict = await llm_verify(
                    self._llm,
                    state.task,
                    state.plan,
                    fresh,
                    await self._screenshots(),
                    state.notes,
                    state.steps,
                    doubted=check.doubted,
                    invented=sorted(state.invented),
                    config=self._config,
                    ledger=state.ledger,
                )
                state.ledger.record(verdict.cost)
                accepted = _verified(verdict.data, state.plan, state.notes, state.invented)
                # Asked to open httpx's project page, a run picked DONE on pypi.org before acting, Jev held the one
                # requirement unmet, and flash-lite called it complete. Only an action can do what Jev says is
                # undone, so a run that has taken none cannot be talked past it. The history, not the steps, since
                # an opened shortcut acts too: counting only steps held a run already on /project/httpx idle, and
                # its recovery clicked through to GitHub.
                idle = not any(
                    h.outcome is StepOutcome.EXECUTED and h.operation not in {*_NOT_ACTING, Operation.ESCALATE}
                    for h in state.history
                )
                undone = {r.id for r in state.plan.requirements if r.kind is RequirementKind.ACTION} & set(check.unmet)
                accepted = accepted and not (idle and undone)
                missing, misread = verdict.data.missing, _misread(verdict.data, state.plan, state.notes, state.invented)
                trace(
                    "verify",
                    complete=verdict.data.complete,
                    missing=list(verdict.data.missing),
                    ungrounded=list(verdict.data.ungrounded),
                )
            if accepted and until is not None:
                accepted = await until((self._raw_observation or fresh).url)
            if not accepted:
                requirements = {r.id: r.text for r in state.plan.requirements}
                unmet = [
                    f"{key}: {requirements.get(key, key)}"
                    for key in dict.fromkeys((*check.unmet, *missing))
                    if key not in misread
                ]
                # Naming the page is what gives recovery something to do: told only "completion not confirmed",
                # it sent the run to finish again from the same notes until the recoveries ran out.
                unmet += [
                    f"{key}: {requirements[key]} (read off "
                    + ", ".join(dict.fromkeys(f.evidence.url for _, f in state.notes.supporting(key) if f.evidence))
                    + ", an address built from the task, not the page it describes)"
                    for key in sorted(misread)
                ]
                state.notes.unevidence(misread)
                reason = self._redactor.redact(f"DONE rejected: {'; '.join(unmet) or 'completion not confirmed'}")
                state.history.append(
                    HistoryEntry(
                        operation=Operation.DONE,
                        target=None,
                        outcome=StepOutcome.FAILED,
                        page_changed=False,
                        effect=reason,
                    )
                )
                # The verifier only runs when Jev's done check doubts; otherwise Jev's verdict is the last word.
                judge = Decider.LLM if check.verdict is DoneVerdict.VERIFY else Decider.JEV
                await self._record_failure(state, observation, Operation.DONE, reason, decided_by=judge)
                await self._recover(state, observation, reason)
                return None
            handed, drafting = drafting, None
            return await self._conclude(state, output_schema, check.answer or handed)
        finally:
            if drafting is not None:
                await _discard(drafting)

    async def _answer(self, state: _RunState, prepared: _Prepared) -> tuple[ComposedAnswer, bool]:
        """The answer and whether its claims held, composing only when nothing prepared survives the check."""
        if isinstance(prepared, ComposedAnswer):
            if (held := await self._holds(state, prepared)) is not None:
                return held, True
            # Jev took the reader's facts as the answer and then doubted a claim in them, which is what
            # the composer exists for.
            prepared = None
        facts = draft_answer(state.plan, state.notes)
        try:
            composed = (
                await (
                    prepared
                    if prepared is not None
                    else compose(
                        self._llm, state.task, state.plan, state.notes, tokens=self._config.tokens, ledger=state.ledger
                    )
                )
            ).data
        except LLMError:
            # A composer that fails (one looped to its output cap) leaves the reader's facts, each with its quote,
            # which is an answer the claim check can still pass; failing the run over it threw away read evidence.
            if facts is None:
                raise
            logger.warning("The composer failed; offering the reader's facts to the claim check", exc_info=True)
            composed, facts = facts, None
        held = await self._holds(state, composed)
        if held is None and facts is not None:
            # A list of forty records came back as one claim citing one quote, which no claim check should pass. The
            # reader's own facts each carry the quote that shows them, so they are offered to the same check.
            held = await self._holds(state, facts)
        return held or composed, held is not None

    async def _holds(self, state: _RunState, answer: ComposedAnswer) -> ComposedAnswer | None:
        return await check_claims(
            self._jev, answer, state.notes, self._config.thresholds, tokens=self._config.tokens, ledger=state.ledger
        )

    async def _extraction(self, state: _RunState, output_schema: type[BaseModel]) -> Extraction:
        """The caller's schema, filled from a capture taken inside this branch so it overlaps the answer."""
        return await extract(
            self._jev,
            self._llm,
            state.task,
            await self._capture(),
            output_schema,
            notes=state.notes,
            tokens=self._config.tokens,
            ledger=state.ledger,
        )

    async def _conclude(
        self,
        state: _RunState,
        output_schema: type[BaseModel] | None,
        prepared: _Prepared = None,
    ) -> RunResult:
        answer: str | None = None
        composed: ComposedAnswer | None = None
        citations: tuple[Citation, ...] = ()
        data: JsonValue | None = None
        evidence: list[Evidence] = [fact.evidence for fact in state.notes.facts if fact.evidence is not None]
        verified = True
        # Writing the answer and filling the caller's schema read the same finished notes and neither
        # needs the other's output, so a task that wants both pays for the slower one rather than both.
        answering = self._answer(state, prepared) if state.plan.answer_expected else None
        extracting = self._extraction(state, output_schema) if output_schema is not None else None
        if answering is not None and extracting is not None:
            first, second = asyncio.create_task(answering), asyncio.create_task(extracting)
            try:
                (composed, verified), extraction = await asyncio.gather(first, second)
            finally:
                # gather reports the first failure and leaves its sibling running, which would go on
                # calling a model after the run had already failed or hit its budget.
                for task in (first, second):
                    if not task.done():
                        await _discard(task)
        elif answering is not None:
            composed, verified = await answering
            extraction = None
        else:
            extraction = await extracting if extracting is not None else None
        if composed is not None:
            answer, citations = self._public_answer(composed)
        if extraction is not None:
            data = extraction.data
            evidence.extend(extraction.evidence)
            verified = verified and extraction.problem is None
        status = Status.COMPLETE if verified else Status.UNVERIFIED
        # The answer's notes and each schema field cite independently, so one quote backing a package name, its
        # version and the answer came back three times; the same words on the same page are one citation.
        cited: dict[tuple[str, str], Evidence] = {}
        for item in evidence:
            cited.setdefault((item.url, item.quote), item)
        return self._result(
            state, state.ledger, status, answer=answer, data=data, evidence=tuple(cited.values()), citations=citations
        )

    def _public_fact(self, fact: Fact) -> StepFact:
        redact = self._redactor.redact
        text = redact(fact.text)
        requirement_id = redact(fact.requirement_id) if fact.requirement_id is not None else None
        if fact.evidence is None:
            return StepFact(text=text, requirement_id=requirement_id, reader=fact.reader)
        url, quote = self._redactor.redact_url(fact.evidence.url), redact(fact.evidence.quote)
        return StepFact(
            text=text,
            requirement_id=requirement_id,
            quote=quote,
            url=url,
            reader=fact.reader,
            deep_link=text_fragment(url, quote),
        )

    def _public_answer(self, composed: ComposedAnswer) -> tuple[str, tuple[Citation, ...]]:
        """The answer and its citations with secrets redacted, links included.

        A link percent-encodes its quote, where redacting the text cannot see a secret, so each link is rebuilt
        from the redacted address and quote, and only the prose between links is redacted as text: redacting a
        rebuilt link again would rewrite a secret the site published in its own hostname.
        """
        redact = self._redactor.redact
        citations = []
        links: dict[str, str] = {}
        for citation in composed.citations:
            url, quote = self._redactor.redact_url(citation.url), redact(citation.quote)
            public = citation.model_copy(
                update={
                    "text": redact(citation.text),
                    "url": url,
                    "quote": quote,
                    "deep_link": text_fragment(url, quote),
                }
            )
            links[citation.deep_link] = public.deep_link
            citations.append(public)
        # Every link destination in one pass. Replacing one link at a time rewrote the start of any longer link
        # sharing its prefix, which then no longer matched and kept its percent-encoded secret. Split keeps each
        # destination at an odd index, between the prose around it.
        pieces = ANSWER_LINK.split(composed.linked_answer)
        answer = "".join(f"](<{links[piece]}>)" if n % 2 else redact(piece) for n, piece in enumerate(pieces))
        return answer, tuple(citations)

    def _plan_mark(self, state: _RunState) -> str:
        """What the plan still wants. Requirement ids, not model prose: this asks whether the run resolved
        anything, and a plan the agent cannot influence by rewording is the only honest way to ask it."""
        if state.ready_plan is None:
            return ""
        return ",".join(sorted(r.id for r in state.notes.unresolved(state.ready_plan)))

    def _tripwires(self, state: _RunState) -> list[Tripped]:
        """Every grinding signal that holds right now, the long-standing unchanged-page count first."""
        stall = self._config.stall
        tripped: list[Tripped] = []
        if state.unchanged >= stall.unchanged_actions:
            tripped.append(Tripped(Tripwire.NO_PROGRESS, state.unchanged))
        repeated = repeated_action(state.history[state.recovered_at :], stall.repeated_actions)
        if repeated is not None:
            tripped.append(repeated)
        # An empty mark means the plan was not written yet, and every step before it would look identical.
        if state.ready_plan is not None:
            stagnant = stagnant_plan([mark for mark in state.plan_marks if mark], stall.stagnant_plan_steps)
            if stagnant is not None:
                tripped.append(stagnant)
        return tripped

    def _context(
        self, state: _RunState, secrets: tuple[str, ...], *, check_login: bool, check_bot: bool
    ) -> StepContext:
        unread = None
        if (plan := state.ready_plan) is not None:
            unread = tuple(r.text for r in state.notes.unresolved(plan) if r.kind is RequirementKind.INFORMATION)
            if not unread and _unread(plan, state.notes):
                unread = (state.task,)
        return StepContext(
            task=state.task,
            subgoal=state.hint,
            requirements=tuple(r.text for r in state.ready_plan.requirements) if state.ready_plan else (),
            notes=state.notes.render(self._config.observation.working_notes_chars),
            history=_history(state.history, self._config.observation),
            recovery_memory=_recovery_memory(state, self._config.stall.max_recoveries, self._redactor),
            check_login=check_login,
            check_bot=check_bot,
            has_attachments=bool(state.attachments),
            secrets=secrets,
            unread_requirements=unread,
        )

    def _result(
        self,
        state: _RunState | None,
        ledger: Ledger,
        status: Status,
        *,
        answer: str | None = None,
        data: JsonValue | None = None,
        evidence: tuple[Evidence, ...] = (),
        citations: tuple[Citation, ...] = (),
        error: str | None = None,
    ) -> RunResult:
        return RunResult(
            status=status,
            answer=answer,
            data=data,
            evidence=evidence,
            citations=citations,
            steps=tuple(state.steps) if state else (),
            cost=ledger.breakdown(),
            artifacts=self._page.artifacts[self._artifact_start :],
            final_url=self._redactor.redact_url(state.last_page[0]) if state and state.last_page else None,
            error=error,
            would_fire=tuple(state.would_fire) if state else (),
        )


def _unread(plan: Plan, notes: Notes) -> bool:
    # A plan can file a question under an action ("find the quote using the search form"), and a run that
    # owes an answer with nothing read would hand the composer empty notes: one did, and ended complete on "".
    unresolved = any(r.kind is RequirementKind.INFORMATION for r in notes.unresolved(plan))
    return unresolved or (plan.answer_expected and not notes.facts)


def _answered(plan: Plan, notes: Notes) -> bool:
    """Whether the plan asks to find something and every such requirement is evidenced.

    Not the same as nothing being left unresolved: a plan can file "open the next page" as an action, which no note
    evidences, and a task whose whole point is to turn pages has nothing to find, so it is never answered.
    """
    asked = [r for r in plan.requirements if r.kind is RequirementKind.INFORMATION]
    return bool(asked) and all(notes.evidenced(r.id) for r in asked)


def _drew_something(observation: Observation) -> bool:
    """Whether this page has anything on it for a run to act on or read."""
    return bool(observation.controls or observation.viewport_text.strip())


def _read_exhausted(state: _RunState) -> str:
    missing = "; ".join(
        f"{r.id}: {r.text}" for r in state.notes.unresolved(state.plan) if r.kind is RequirementKind.INFORMATION
    )
    return (
        f"Still missing evidence for {missing or state.task}. "
        "Re-reading this unchanged content will not supply it; find additional content."
    )


def _describe(control: Control) -> str:
    """Name which one was chosen, not just what it read: a label alone cannot identify one of six
    identically labelled buttons, in the step log or in the history the next choice is made from."""
    return f"{control.label} ({control.context})" if control.context else control.label


def _place(url: str) -> tuple[str, str]:
    """An address as origin and path: the query and fragment a site writes back do not make it another page."""
    return origin_of(url), urlsplit(url).path or "/"


def _guessed(plan: Plan, notes: Notes, invented: Set[str]) -> set[str]:
    """The requirements with a fact read on an address the run built from the task rather than clicked to."""
    places = {_place(url) for url in invented}
    return {
        r.id
        for r in plan.requirements
        if any(f.evidence is not None and _place(f.evidence.url) in places for _, f in notes.supporting(r.id))
    }


def _misread(verdict: LLMVerdict, plan: Plan, notes: Notes, invented: Set[str]) -> set[str]:
    """The requirements the verifier says were read off the wrong page, where that page was a guessed address.

    A guessed address is the one that can land on a page of the right shape and the wrong search: a proposed
    address opened a flights summary, the reader quoted a price from it, and the requirement counted as cited.
    A page reached by clicking was chosen off the site itself, so there the verifier's doubt is only doubt.
    """
    return set(verdict.ungrounded) & _guessed(plan, notes, invented)


def _verified(verdict: LLMVerdict, plan: Plan, notes: Notes, invented: Set[str]) -> bool:
    """Whether the verifier's doubts leave the run finished.

    An information requirement the notes cite facts for is not left open by the verifier: the answer's claims are
    checked against those quotes before it is given. On flash-lite the verifier named "compare the two release
    dates" missing with both dates in the notes, one run in three, until the run stopped stuck. Only evidence read
    on a guessed address is not excused, since that is the wrong page the citing check cannot see.
    """
    cited = {r.id for r in plan.requirements if r.kind is RequirementKind.INFORMATION and notes.evidenced(r.id)}
    misread = _misread(verdict, plan, notes, invented)
    named = set(verdict.missing) | (set(verdict.ungrounded) & {r.id for r in plan.requirements})
    doubted = named - (cited - misread)
    return not doubted and (verdict.complete or bool(named))


def _described(entry: HistoryEntry) -> str:
    return f"{entry.operation.value if entry.operation else 'open'} {entry.target or ''}".strip()


def _history(history: Sequence[HistoryEntry], limits: ObservationLimits) -> tuple[HistoryEntry, ...]:
    """The recent actions in full, after the earlier ones without the effects that make an entry long."""
    split = len(history) - limits.history_entries
    earlier = history[max(0, split - limits.earlier_history_entries) : max(0, split)]
    return (*(entry.model_copy(update={"effect": None}) for entry in earlier), *history[max(0, split) :])


def _recovery_text(text: str) -> str:
    return " ".join(text.split())[:_RECOVERY_CHARS]


def _recovery_memory(state: _RunState, limit: int, redactor: Redactor) -> str:
    """One bounded account for both models, without presenting a proposed subgoal as an executed action."""
    if not state.recovery_log and not state.recoveries:
        return ""
    # Redacted again on the way out: a secret resolved after a record was written is still blanked.
    return redactor.redact("\n".join((f"recovery {state.recoveries} of {limit} in this stall", *state.recovery_log)))


def _controls_text(observation: Observation) -> str:
    return json.dumps(
        [
            {
                "index": index,
                **control.model_dump(
                    mode="json",
                    include={"label", "context", "role", "value", "operations", "selected", "expanded", "blocking"},
                    exclude_none=True,
                ),
            }
            for index, control in enumerate(observation.controls)
        ]
    )


def _signature(decision: Decision, observation: Observation) -> Signature:
    label = _describe(decision.target) if decision.target else decision.tab_id
    return decision.operation, label, state_key(observation)


def read_question(task: str, wanted: Sequence[Requirement], *, began_at: str | None = None) -> str:
    """The reader's question. `began_at` is the run's first page, given once a list may run past one page:
    "this page and the next" was written there, and read from the second page it would otherwise mean the second
    and the third, or never say the list ends."""
    # Unresolved requirements may refer to an earlier one; keep the task's constraints in every read.
    question = task + "\n\nRequirements still to evidence:\n" + "\n".join(f"- {r.text}" for r in wanted)
    if began_at is None:
        return question
    return (
        f'{question}\n\nThe task\'s "this page" is {began_at}, where the run began, and "the next page" is the one '
        "after it. A task that names how many pages it covers ends at the last one it names."
    )


def next_page_notice(following: Control | None) -> str:
    # The capture is text: a "Next" link reads the same as any other word unless the page's controls say so.
    if following is None:
        return ""
    return (
        f"This page has a next-page control ({following.label!r}): a list on it may continue. A requirement "
        "about the page that control opens is not answered by this page, whatever this page holds: leave it "
        "unanswered until that page is read."
    )


def next_page_control(observation: Observation) -> Control | None:
    """The one control that opens the next page of a list on this page, or None when there is none or doubt.

    Only a link to another address counts: a load-more button leaves the earlier records in the page, and reading
    it again would count them twice. A pager drawn above and below the list is one control for this purpose.
    """
    here = urlsplit(observation.url)
    found: dict[str, Control] = {}
    for control in observation.controls:
        if not pager_link(control) or control.href is None:
            continue
        # The snapshot gives a same-site link as its path and query, and another site's as host and path.
        if control.href.startswith("/"):
            target = urlsplit(urljoin(observation.url, control.href))
            if (target.path, target.query) == (here.path, here.query):
                continue
        found.setdefault(control.href, control)
    return next(iter(found.values())) if len(found) == 1 else None


def _paging(state: _RunState, observation: Observation) -> Decision | None:
    """The step code takes to walk a list the reader needs whole: open its next page, then read what that opened.

    The control is found again on the observation the click will be dispatched against, rather than kept from the
    one that was read, so a page that redrew its pager between the two is still followed.
    """
    if state.next_page:
        state.next_page = False
        target = next_page_control(observation)
        if target is None:
            return None
        state.pages += 1
        state.paged_from = state_key(observation)
        return _code_decision(Operation.CLICK, target)
    if (left := state.paged_from) is not None:
        state.paged_from = None
        # A click that did not open anything new leaves nothing to read; Jev decides from here.
        if state_key(observation) != left and not state.read_here:
            return _code_decision(Operation.READ, None)
    return None


def _code_decision(operation: Operation, target: Control | None) -> Decision:
    return Decision(
        operation=operation,
        target=target,
        tab_id=None,
        operation_confidence=1.0,
        target_confidence=None if target is None else 1.0,
        login_required=None,
        offered_controls=0,
        reduction=Reduction.NONE,
        cost=(),
        input_tokens=0,
    )


def _try_unsure(state: _RunState, observation: Observation) -> bool:
    """Whether to act on Jev's unsure pick rather than recover: once per page state.

    Recovery costs about 5s, and on a flights form 9 of 12 recoveries for an unsure step named the control Jev
    had already picked. Acting costs one step when the pick is wrong, and a wrong pick is still caught: one that
    changes nothing leaves Jev unsure on the same state, which then recovers, one that goes round is caught by
    the revisit check, and one that may commit something irreversible is asked about before it dispatches.
    """
    key = state_key(observation)
    if key in state.tried_unsure:
        return False
    state.tried_unsure.add(key)
    return True


def _follow_recovery(
    state: _RunState, observation: Observation, decision: Decision, *, uncertain: bool
) -> Decision | None:
    """The action recovery named, when Jev is unsure or repeats a failed read and the control still offers it.

    Recovery names one action on one control, or on the page itself. Handed back to Jev only as a hint, it left
    Jev choosing between two Search buttons at 0.49 until the recovery budget ran out, the named action never taken.
    """
    directed, state.directed = state.directed, None
    if not uncertain or directed is None:
        return None
    operation, control_id = directed
    if control_id is None:
        return decision.model_copy(update={"operation": operation, "target": None, "directed": True})
    target = next((c for c in observation.controls if c.id == control_id and operation in c.operations), None)
    if target is None:
        return None
    return decision.model_copy(update={"operation": operation, "target": target, "directed": True})


async def _discard[T](task: asyncio.Task[T]) -> None:
    """Cancel abandoned work and wait for it to stop, so nothing bills a run that has already ended.

    Waiting is the point. Cancellation is a request, and a task that has already entered an HTTP call
    does not stop until it is next at an await, so returning without joining leaves the call in flight.
    """
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)


def _require(target: Control | None) -> Control:
    if target is None:
        raise JevError("operation needs a target and Jev offered none")
    return target
