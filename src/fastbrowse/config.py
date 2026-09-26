"""Tunable policy. Defaults are starting points to be recalibrated from recorded decision packets."""

from collections.abc import Sequence

from pydantic import Field

from fastbrowse.models import Frozen, TripwireMode


class Thresholds(Frozen):
    recover_below: float = 0.55
    """Operation/target confidence under this waits for the plan, then goes to LLM recovery if the step acts."""
    sensitive_act_from: float = 0.90
    """Minimum confidence for an action code has classified as irreversible (authorization also required)."""
    login_required_above: float = 0.70
    bot_check_above: float = 0.70
    """Jev's agreement, on a page it also judged a wall, that the wall is a bot check and not a sign-in."""
    irreversible_above: float = 0.50
    done_accept_from: float = 0.85
    done_confirmed_from: float = 0.50
    """With every action requirement confirmed, the holistic done answer that accepts without the LLM verifier.
    Jev scores a page that is right 0.49 to 0.91 as done and a near miss (the organisation instead of the
    repository, search results, a related article) 0.04 or less, so doubt between the two is not a near miss."""
    requirement_confirmed_below: float = 0.30
    """A requirement's "not satisfied" answer under which it counts as confirmed; near misses scored 0.65 or more."""
    claim_problem_above: float = 0.70
    """Any "yes = something is wrong" answer check above this rejects the claim."""
    value_stated_above: float = 0.50
    """Jev's agreement that the task or notes hold a field's value, under which a NEEDS_INPUT stop stands."""
    rewrite_from: float = 0.30
    """Jev's doubt that the read facts already answer the task, from which the LLM composer writes it instead."""
    rewrite_one_claim_from: float = 0.60
    """The same doubt for a draft of one fact, which cannot repeat itself or leave a comparison between its facts
    undone. Jev doubted one-fact drafts 0.15 to 0.51 and every draft of more 0.69 to 0.91; the composer only
    reworded the one-fact ones (Argo, "Hello World!"), at 2 to 5s each."""


class ObservationLimits(Frozen):
    max_controls: int = Field(default=320, gt=0)
    """The browser's hard pool bound; late controls beyond it are still cut in DOM order."""
    max_offscreen_controls: int = Field(default=120, ge=0)
    """Long footers must leave most of the control budget for what is on screen."""
    max_offered_controls: int = Field(default=160, gt=0)
    """Jev's step sees at most this many controls; on a denser page a relevance filter picks which.

    Typesafe documents that Jev 1.13 loses accuracy on large state full of irrelevant detail, and a
    DOM-order cut dropped the control a task needed.
    """
    viewport_text_chars: int = Field(default=6000, gt=0)
    """Keep navigation's page excerpt small; the reader captures the whole page when it needs more."""
    working_notes_chars: int = Field(default=6000, gt=0)
    """Navigation, field writing and recovery share a short memory; verdicts use the token budget instead."""
    history_entries: int = Field(default=6, ge=0)
    """Keep effects for six recent actions; older effects describe page states that have already changed."""
    earlier_history_entries: int = Field(default=14, ge=0)
    """Actions before the recent ones, shown without their effects. With only the last six, a form filled in
    eight steps lost its first field from view, and Jev typed the origin again instead of searching."""
    group_size: int = Field(default=30, gt=1)
    """Thirty controls keep the second choice small when selection goes group -> element."""
    max_choice_options: int = Field(default=240, gt=1, le=255)
    """Leave headroom under Jev's 255-option ceiling for policy choices."""


class TokenBudget(Frozen):
    state_plus_largest_question: int = Field(default=24_000, gt=0)
    """Conservative target under Jev's 32k limit, since tokens are estimated locally."""
    state_plus_all_questions: int = Field(default=48_000, gt=0)
    """Conservative target under Jev's 64k limit."""
    batch_tokens: int = Field(default=8000, gt=0)
    """Target for one request among many independent questions. The gateway sheds large requests with 503s (a 26k
    token request failed five of eight times, 4k never), so a batch of questions is split small and sent in parallel."""
    chars_per_token: float = Field(default=3.0, gt=0)
    """Allow more tokens per character than ordinary English to cover JSON and identifiers."""
    read_output_tokens: int = Field(default=8000, gt=0)
    """A page of records needs room for each value and its verbatim quote in the reader's JSON."""
    compose_output_tokens: int = Field(default=8000, gt=0)
    """A long answer repeats citation ids for every claim, so it needs more room than a planning response."""

    def remaining_chars(self, context: str, questions: Sequence[str] = ()) -> int:
        """Room for notes after other content, using the same conservative input targets for LLM prompts."""
        sizes = [len(question) for question in questions]
        return max(
            0,
            int(
                min(
                    self.state_plus_largest_question * self.chars_per_token - max(sizes, default=0),
                    self.state_plus_all_questions * self.chars_per_token - sum(sizes),
                )
            )
            - len(context),
        )


class StallRules(Frozen):
    unchanged_actions: int = Field(default=3, gt=0)
    max_recoveries: int = Field(default=2, ge=0)
    repeated_actions: int = Field(default=3, gt=1)
    """Times one interaction on one target with one value may recur before `tripwires` has an opinion."""
    barren_reads: int = Field(default=2, gt=0)
    """Reads of one page state, for one set of open requirements, that may add no fact before the run has to act
    instead. A page that rewrites its own text on every observation (a ticker, rotating ads, a live counter)
    otherwise mints a read key it has never seen each time, and can be read until the step budget runs out."""
    stagnant_plan_steps: int = Field(default=4, gt=1)
    """Steps the unresolved requirements may stay exactly the same for. Above `unchanged_actions`, because a
    page can legitimately take several moves -- opening a menu, filling a field -- before it evidences one."""
    tripwires: TripwireMode = TripwireMode.SHADOW
    """Whether `repeated_actions` and `stagnant_plan_steps` recover or only log what they would have done.

    Shadow by default: both are new, and a tripwire that ends a run which was about to succeed costs more
    than one that never fires. The eval suites report every would-fire, which is the evidence for arming."""


class Config(Frozen):
    thresholds: Thresholds = Thresholds()
    observation: ObservationLimits = ObservationLimits()
    tokens: TokenBudget = TokenBudget()
    stall: StallRules = StallRules()
    refuse_cookie_banners: bool = True
    """Refuse cookie consent on the platforms DuckDuckGo's autoconsent knows, before the banner paints."""
    step_frames: bool = False
    """Send a PNG of the page with every step event, for a caller that shows the run as it happens.

    Off by default: it costs a screenshot round trip per step, which a caller with nowhere to put the image
    would pay for nothing. A step whose page is showing a resolved secret sends no frame.
    """
    max_upload_bytes: int = Field(default=25 * 1024 * 1024, gt=0)
    """Uploads move as bytes over the DevTools socket; larger files return NEEDS_INPUT with a reason."""
    max_pages: int = Field(default=12, ge=0)
    """Next pages of a list code opens and reads by itself in one run; further pages are left to Jev's choice.

    The ledger already bounds steps, calls and dollars, so this is not there to keep a run inside its budget. It
    is there because walking a list is the one thing the loop does without asking a model each time: a catalogue
    of fifty pages would spend the whole step budget on paging before anything noticed, and the run would end
    budget_exceeded rather than saying it could not read the list.
    """
