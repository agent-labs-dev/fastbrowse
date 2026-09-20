"""Gates code owns: irreversible actions need authorization, and secrets never leave their origins or reach logs.

Models only ever see secret names. Values are resolved here, at dispatch time, for the origin being acted on.
"""

import json
import re
from collections.abc import Mapping
from urllib.parse import quote, quote_plus, urlsplit

from fastbrowse.jev import NoulQuestion
from fastbrowse.models import Operation, SecretRef, SecretResolver
from fastbrowse.page import Control

# A link can still commit through its label ("Delete"), so the words are checked on every control.
_IRREVERSIBLE_WORDS = re.compile(
    r"\b(buy|purchase|pay|checkout|order|confirm|submit|send|delete|remove|erase|wipe|destroy|transfer|"
    r"book|reserve|subscribe|unsubscribe|cancel|publish|post|donate|sign up|register|agree|accept|withdraw|"
    r"close account|deactivate|archive|merge|approve)\b",
    re.IGNORECASE,
)
_DISPATCHING = frozenset({Operation.CLICK, Operation.ENTER})


def may_be_irreversible(operation: Operation, control: Control | None) -> bool:
    """Whether Jev is asked before this dispatches. Only plain navigation is exempt.

    A label list cannot be complete ("Place your order", "Erase"), so every button is asked about: a button
    runs script, and script can commit anything. A link with an href navigates, which is exempt unless its
    label says otherwise.
    """
    if operation not in _DISPATCHING or control is None:
        return False
    if _IRREVERSIBLE_WORDS.search(control.label) or control.input_type == "submit":
        return True
    if operation is Operation.ENTER:
        return control.submit_semantics is not None
    return control.href is None


def irreversible_question(task: str, operation: Operation, control: Control) -> NoulQuestion:
    return NoulQuestion(
        instructions=(
            f"The agent is about to {operation.value} the element labelled {control.label!r} while doing this task: "
            f"{task}\n"
            + (f"It sits under {control.context!r} on the page.\n" if control.context else "")
            + (f"Enter submits this form: {control.submit_semantics}\n" if operation is Operation.ENTER else "")
            + "Would doing so commit something that cannot be undone, such as spending money, sending a "
            "message, submitting an application, or deleting or publishing data?"
        ),
        true="It commits an irreversible or externally visible change.",
        false="It only navigates, filters, reveals or edits a draft that can still be changed.",
    )


_DEFAULT_PORTS = {"http": 80, "https": 443}


def origin_of(url: str) -> str:
    """The web origin, with a port the scheme implies dropped rather than carried.

    `https://shop.example.com` and `https://shop.example.com:443` are one origin, and a secret declared for one
    must be typed on the other: a browser writes the port back either way after a navigation, and comparing the
    two as strings dropped the secret and ended the run at needs_login. Credentials in the URL are dropped too,
    so `https://user@host` cannot pass itself off as another origin.
    """
    parts = urlsplit(url.strip())
    try:
        host, port = (parts.hostname or "").lower(), parts.port
    except ValueError:
        # A port that is not a number: not an origin this can normalize, and never one a secret is declared for.
        return f"{parts.scheme}://{parts.netloc}".lower()
    if not host:
        return f"{parts.scheme}://{parts.netloc}".lower()
    if port == _DEFAULT_PORTS.get(parts.scheme.lower()):
        port = None
    return f"{parts.scheme.lower()}://{host}" + (f":{port}" if port else "")


def secret_allowed(ref: SecretRef, origin: str) -> bool:
    return origin.lower() in {o.lower().rstrip("/") for o in ref.origins}


async def resolve_secret(resolver: SecretResolver, name: str, origin: str) -> str | None:
    """Only a secret declared for this origin resolves; anything else is treated as missing."""
    ref = next((r for r in resolver.available() if r.name == name), None)
    if ref is None or not secret_allowed(ref, origin):
        return None
    return await resolver.resolve(name, origin)


class ScopedSecrets:
    """Secret values held in this process, each usable only on one origin: the `SecretResolver` for a run."""

    def __init__(self, values: Mapping[str, str], origin: str) -> None:
        self._values = dict(values)
        self._origin = origin

    def available(self) -> tuple[SecretRef, ...]:
        return tuple(SecretRef(name=name, origins=(self._origin,)) for name in self._values)

    async def resolve(self, name: str, origin: str) -> str | None:
        return self._values.get(name) if origin == self._origin else None


class Redactor:
    """Replaces every resolved secret value with its name in anything written out of the process."""

    def __init__(self) -> None:
        self._values: dict[str, str] = {}

    def register(self, name: str, value: str) -> None:
        # Pages and logs carry a value encoded as often as raw: in a query string, or escaped inside JSON.
        forms = {
            value,
            quote(value, safe=""),
            quote_plus(value),
            json.dumps(value)[1:-1],
            json.dumps(value, ensure_ascii=False)[1:-1],
        }
        for form in forms:
            if form:
                self._values[form] = name

    def redact(self, text: str) -> str:
        # Longest first, so a secret containing another secret is replaced whole.
        for value in sorted(self._values, key=len, reverse=True):
            text = text.replace(value, f"[secret:{self._values[value]}]")
        return text

    def mask(self, text: str) -> str:
        """Blank secret values at equal length, so offsets into the text (capture blocks) stay valid."""
        for value in sorted(self._values, key=len, reverse=True):
            text = text.replace(value, "•" * len(value))
        return text

    def reveals(self, text: str) -> bool:
        return any(value in text for value in self._values)
