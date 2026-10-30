"""Tunable policy. Defaults are starting points to be recalibrated from recorded decision packets."""

from pydantic import Field

from fastbrowse.models import Frozen


class Thresholds(Frozen):
    recover_below: float = 0.55
    """Operation/target confidence under this waits for the plan, then goes to LLM recovery if the step acts."""
    sensitive_act_from: float = 0.90
    """Minimum confidence for an action code has classified as irreversible (authorization also required)."""
    login_required_above: float = 0.70
    bot_check_above: float = 0.70
    """Jev's agreement, on a page it also judged a wall, that the wall is a bot check and not a sign-in."""
    irreversible_above: float = 0.50
    requested_above: float = 0.50
    """Jev's agreement that the task asks for an authorized commit, above which it proceeds under sensitive_act_from."""
    claim_problem_above: float = 0.70
    """A requirement's "not satisfied" answer above this keeps the run working."""
    value_stated_above: float = 0.50
    """Jev's agreement that the task or notes hold a field's value, under which a NEEDS_INPUT stop stands."""
    rewrite_from: float = 0.30
    """Jev's doubt that the read facts already answer the task, from which the LLM composer writes it instead."""


class ObservationLimits(Frozen):
    group_size: int = Field(default=30, gt=1)
    """Thirty controls keep the second choice small when selection goes group -> element."""


class TokenBudget(Frozen):
    state_plus_largest_question: int = Field(default=32_000, gt=0)
    """Tokens a decision request may spend on its state and largest question: Jev's documented 32k, brought within
    another model's limit by the model's decision profile."""
    state_plus_all_questions: int = Field(default=64_000, gt=0)
    """Tokens a decision request may spend on its state and every question: Jev's documented 64k."""
    batch_tokens: int = Field(default=8000, gt=0)
    """Target for one request among many independent questions. The gateway sheds large requests with 503s (a 26k
    token request failed five of eight times, 4k never), so a batch of questions is split small and sent in parallel."""
    chars_per_token: float = Field(default=3.0, gt=0)
    """Local estimate for prose prompts; a decision request's dense JSON is counted at 1.5 characters a token."""
    read_output_tokens: int = Field(default=0, ge=0)
    """The reader's output ceiling; 0 sends none, so a page of records and its verbatim quotes is never cut short."""
    compose_output_tokens: int = Field(default=0, ge=0)
    """The composer's output ceiling; 0 sends none, so a long answer citing every claim is never cut short."""


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
    step_frames: bool = False
    """Send a PNG of the page with every step event, for a caller that shows the run as it happens.

    Off by default: it costs a screenshot round trip per step, which a caller with nowhere to put the image
    would pay for nothing. A step whose page is showing a resolved secret sends no frame.
    """
    max_upload_bytes: int = Field(default=25 * 1024 * 1024, gt=0)
    """Uploads move as bytes over the DevTools socket; larger files return NEEDS_INPUT with a reason."""
    max_download_bytes: int = Field(default=512 * 1024 * 1024, gt=0)
    """Largest download kept as an artifact. A body over it is refused and reported, never silently dropped.

    Downloads arrive whole over the DevTools socket, so this bounds a run's memory as much as a file's size.
    A caller that knows the size of the file it asked for, such as a workspace export, sets it deliberately;
    the run reports the size it saw and the limit it hit rather than losing the file without a word.
    """
    max_pages: int = Field(default=12, ge=0)
    """Next pages of a list code opens and reads by itself in one run; further pages are left to Jev's choice.

    Caller supplied ceilings bound steps, calls and dollars, so this is not there to keep a run inside its budget.
    It is there because walking a list is the one thing the loop does without asking a model each time: a catalogue
    of fifty pages would otherwise spend the caller's paging allowance before anything noticed, and an explicitly
    capped run would end budget_exceeded rather than saying it could not read the list.
    """
