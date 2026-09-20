"""Tunable policy. Defaults are starting points to be recalibrated from recorded decision packets."""

from pydantic import Field

from fastbrowse.models import Frozen


class Thresholds(Frozen):
    recover_below: float = 0.55
    """Operation/target confidence under this waits for the plan, then goes to LLM recovery if the step acts."""
    sensitive_act_from: float = 0.90
    """Minimum confidence for an action code has classified as irreversible (authorization also required)."""
    login_required_above: float = 0.70
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


class ObservationLimits(Frozen):
    max_controls: int = Field(default=160, gt=0)
    max_offscreen_controls: int = Field(default=40, ge=0)
    viewport_text_chars: int = Field(default=6000, gt=0)
    history_entries: int = Field(default=6, ge=0)
    earlier_history_entries: int = Field(default=14, ge=0)
    """Actions before the recent ones, shown without their effects. With only the last six, a form filled in
    eight steps lost its first field from view, and Jev typed the origin again instead of searching."""
    group_size: int = Field(default=30, gt=1)
    """Controls per group when a choice exceeds `max_choice_options` and selection goes group -> element."""
    max_choice_options: int = Field(default=240, gt=1, le=255)


class TokenBudget(Frozen):
    state_plus_largest_question: int = Field(default=24_000, gt=0)
    """Conservative target under Jev's 32k limit, since tokens are estimated locally."""
    state_plus_all_questions: int = Field(default=48_000, gt=0)
    """Conservative target under Jev's 64k limit."""
    chars_per_token: float = Field(default=3.0, gt=0)


class StallRules(Frozen):
    unchanged_actions: int = Field(default=3, gt=0)
    max_recoveries: int = Field(default=2, ge=0)


class Config(Frozen):
    thresholds: Thresholds = Thresholds()
    observation: ObservationLimits = ObservationLimits()
    tokens: TokenBudget = TokenBudget()
    stall: StallRules = StallRules()
    refuse_cookie_banners: bool = True
    """Refuse cookie consent on the platforms DuckDuckGo's autoconsent knows, before the banner paints."""
    max_upload_bytes: int = Field(default=25 * 1024 * 1024, gt=0)
    """Uploads move as bytes over the DevTools socket; larger files return NEEDS_INPUT with a reason."""
    max_pages: int = Field(default=12, ge=0)
    """Next pages of a list code opens and reads by itself in one run; further pages are left to Jev's choice.

    The ledger already bounds steps, calls and dollars, so this is not there to keep a run inside its budget. It
    is there because walking a list is the one thing the loop does without asking a model each time: a catalogue
    of fifty pages would spend the whole step budget on paging before anything noticed, and the run would end
    budget_exceeded rather than saying it could not read the list.
    """
