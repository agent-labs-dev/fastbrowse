"""One attempt's clock: one rule for every arm and both runners.

`seconds` runs from the browser session being ready to the agent's final answer. Opening and closing the session are
the provider's time, not the agent's: they are recorded apart as `session_open_seconds` and `session_close_seconds`
and never reach a headline figure. A row an arm could not measure this way carries `SESSION_INCLUDED` instead of
`TIMING_BASIS`, and publication refuses it, so a mixed number is never reported under the same label.

No browser setup counts. The whole span from asking for a browser to its first usable page (the browser, connecting to
it, preparing the tab, opening the start page) leaves the agent's clock (`setup_marks`), and is recorded in
`session_open_seconds`. A run that reads over HTTP has no session to open, so its clock starts with the run, and a span
starts only if it then asks for a browser. A run that starts in a browser has the same span from session ready. An arm
whose connection happens where the harness cannot see it (hosted Browser Use, Browser Harness) keeps that connection on
its clock; its row says so by `timing_note`.

The agent has no time limit: from ready to its answer it runs until it ends itself or a provider ends it. A bounded
clock bounds only the provider's two phases around it: `SESSION_OPEN_SECONDS` to get a session and
`SESSION_CLOSE_SECONDS` to read the final page and close. Running past either is an outage, recorded as unavailable and
retried like any other, never an agent failure. A diagnostic's clock bounds nothing.
"""

import asyncio
import time
from collections.abc import Awaitable, Callable
from contextvars import ContextVar
from types import TracebackType
from typing import Literal, Self

TIMING_BASIS = "session-ready-to-answer"
"""What `seconds` measures on a row, recorded on every row so two clocks are never compared."""
SESSION_INCLUDED = "session-included"
"""The basis of a row whose `seconds` could not be separated from opening or closing its browser session."""
SESSION_OPEN_SECONDS = 300.0
"""How long a browser session may take to open before the attempt is an outage. Provider starts take seconds, so this
only bounds a hung one; it is recorded with every run, and publication refuses any other."""
SESSION_CLOSE_SECONDS = 300.0
"""How long an attempt that answered may take to read its final page and close. A cloud browser's stop is retried and
its proxy cost settles about 45 seconds later, so this is well above a working close and only bounds a hung one."""

type Phase = Literal["open", "agent", "close"]


class AttemptClock:
    """The clock of one attempt, entered around everything the attempt does. A `bounded` clock stops a session that
    does not open or close in time; the agent's phase between them is never stopped. A diagnostic's clock is not
    bounded at all."""

    def __init__(self, *, bounded: bool) -> None:
        self.bounded = bounded
        self.began = time.monotonic()
        self.ready_at: float | None = None
        self.answered_at: float | None = None
        self.asked_at: float | None = None
        self.opened = 0.0
        self._timeout = asyncio.timeout(SESSION_OPEN_SECONDS if bounded else None)

    async def __aenter__(self) -> Self:
        self.began = time.monotonic()
        _current.set(self)
        await self._timeout.__aenter__()
        return self

    async def __aexit__(
        self, kind: type[BaseException] | None, error: BaseException | None, trace: TracebackType | None
    ) -> bool | None:
        return await self._timeout.__aexit__(kind, error, trace)

    def _bound(self, seconds: float | None) -> None:
        """Stop the attempt `seconds` from now, or never for None; a diagnostic's clock stays unbounded."""
        if self.bounded and not self._timeout.expired():
            self._timeout.reschedule(None if seconds is None else asyncio.get_running_loop().time() + seconds)

    def ready(self, at: float) -> None:
        """The session is ready: the agent's clock starts now, with no deadline. Later calls change nothing."""
        if self.ready_at is None:
            self.ready_at = at
            self._bound(None)

    def session_opened(self, seconds: float) -> None:
        """Browser setup took `seconds`: the agent's clock does not count them."""
        self.opened += seconds

    def asked(self, at: float) -> None:
        """Browser setup begins: nothing from here to `page_ready` is the agent's."""
        if self.asked_at is None:
            self.asked_at = at

    def page_ready(self, at: float) -> None:
        """The first usable page is open, which ends the setup `asked` began."""
        if self.asked_at is not None:
            self.session_opened(at - self.asked_at)
            self.asked_at = None

    def answered(self, at: float) -> None:
        """The agent's final answer is in: its clock stops, and what is left is closing. Setup that never reached a
        page ended here, with the agent having done nothing since it asked."""
        if self.answered_at is None:
            self.page_ready(at)
            self.answered_at = at
            self._bound(SESSION_CLOSE_SECONDS)

    def expired(self) -> bool:
        return self._timeout.expired()

    @property
    def phase(self) -> Phase:
        return "open" if self.ready_at is None else "agent" if self.answered_at is None else "close"

    @property
    def open_seconds(self) -> float | None:
        return None if self.ready_at is None else round(self.ready_at - self.began + self.opened, 2)

    def seconds(self) -> float:
        """The agent's seconds so far: none until the session is ready."""
        if self.ready_at is None:
            return 0.0
        now = time.monotonic() if self.answered_at is None else self.answered_at
        return (now if self.asked_at is None else self.asked_at) - self.ready_at - self.opened


_current: ContextVar[AttemptClock | None] = ContextVar("attempt_clock", default=None)


def session_ready() -> float:
    """Mark the attempt's session ready, and return the moment."""
    at = time.monotonic()
    if (clock := _current.get()) is not None:
        clock.ready(at)
    return at


def setup_marks() -> tuple[Callable[[], None], Callable[[], Awaitable[None]]]:
    """This attempt's two marks around browser setup: call the first on asking for a browser, and pass the second as
    `run_task`'s `page_ready`. They hold the attempt's clock themselves, as fastbrowse calls back outside its context."""
    clock = _current.get()

    def asked() -> None:
        if clock is not None:
            clock.asked(time.monotonic())

    async def page_ready() -> None:
        if clock is not None:
            clock.page_ready(time.monotonic())

    return asked, page_ready


def setup_seconds() -> float:
    """The browser setup taken off this attempt's clock so far."""
    clock = _current.get()
    return 0.0 if clock is None else clock.opened


def session_answered(at: float | None = None) -> float:
    """Mark the attempt's final answer in, at `at` on `time.monotonic()` when the agent reported when it answered, or
    now, and return the moment."""
    at = time.monotonic() if at is None else at
    if (clock := _current.get()) is not None:
        clock.answered(at)
    return at


def elapsed() -> float:
    """The latest attempt's agent seconds, for a row written when an attempt is cut short."""
    clock = _current.get()
    return 0.0 if clock is None else clock.seconds()
