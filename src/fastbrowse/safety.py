"""Gates code owns: irreversible actions need authorization, and secrets never leave their origins or reach logs.

Models only ever see secret names. Values are resolved here, at dispatch time, for the origin being acted on.
"""

import json
from collections.abc import Mapping, Sequence
from urllib.parse import quote, quote_plus, urlsplit

from fastbrowse.jev import NoulQuestion
from fastbrowse.models import UNTRUSTED, Operation, SecretRef, SecretResolver, SecretValue
from fastbrowse.page import Control


def may_be_irreversible(operation: Operation, control: Control | None) -> bool:
    """Whether Jev is asked before this dispatches: every click and every Enter.

    Whether a click commits is a judgment about the page, so Jev makes it rather than a word list. A list of
    committing labels is never complete ("Place your order", "Yes, I'm sure"), and a link commits as easily
    as a button: a one-click unsubscribe is an href, and a script handler runs whatever the element says.
    """
    if control is None:
        return False
    match operation:
        case Operation.CLICK:
            return True
        # Not only a form's Enter: a chat, comment or DM box sends on Enter through its own script, with no
        # form for the page to describe.
        case Operation.ENTER:
            return True
        # A dialog's accept is asked about where the dialog is handled; the rest change nothing off the page.
        case (
            Operation.HOVER
            | Operation.FILL
            | Operation.SELECT
            | Operation.ESCAPE
            | Operation.SCROLL
            | Operation.BACK
            | Operation.SWITCH_TAB
            | Operation.UPLOAD
            | Operation.DIALOG
            | Operation.READ
            | Operation.DONE
            | Operation.ESCALATE
        ):
            return False


def irreversible_question(task: str, operation: Operation, control: Control) -> NoulQuestion:
    return NoulQuestion(
        instructions=(
            f"{UNTRUSTED}\nThe agent is about to {operation.value} the element labelled {control.label!r} while "
            f"doing this task: {task}\n"
            + (f"It sits under {control.context!r} on the page.\n" if control.context else "")
            + (f"Enter submits this form: {control.submit_semantics}\n" if operation is Operation.ENTER else "")
            + "Would doing so commit something that cannot be undone, such as spending money, sending a "
            "message, submitting an application, or deleting or publishing data?"
        ),
        true="It commits an irreversible or externally visible change.",
        false="It only navigates, filters, reveals or edits a draft that can still be changed.",
    )


_DEFAULT_PORTS = {"http": 80, "https": 443}
# A declared origin whose host starts with this covers that host and everything under it.
_WILDCARD = "*."


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
    # `hostname` unwraps an IPv6 literal, and `https://::1` is not a URL any parser reads back: the colons
    # become a port that is not a number. An origin this returns has to survive being parsed again.
    if ":" in host:
        host = f"[{host}]"
    return f"{parts.scheme.lower()}://{host}" + (f":{port}" if port else "")


def secret_allowed(ref: SecretRef, origin: str) -> bool:
    """Whether this secret may be typed on `origin`.

    A declared origin is an exact one, or one whose host starts with `*.`, for a login that is the same login
    across a site's hosts: `https://*.example.com` covers `www.example.com`, `accounts.example.com` and
    `example.com` itself. The scheme and port must still match, and the wildcard only ever stands for whole
    labels, so it does not cover `example.com.evil.test`, which merely ends with the same letters.
    """
    here = urlsplit(origin_of(origin))
    host = here.hostname or ""
    for declared in ref.origins:
        pattern = urlsplit(origin_of(declared.rstrip("/")))
        # The scheme and port are never wildcarded: a secret for https is not for http, whatever the host.
        if (pattern.scheme, pattern.port) != (here.scheme, here.port):
            continue
        covered = pattern.hostname or ""
        if covered == host:
            return True
        if not covered.startswith(_WILDCARD) or not (suffix := covered[len(_WILDCARD) :]):
            continue
        if host == suffix or host.endswith(f".{suffix}"):
            return True
    return False


async def resolve_secret(resolver: SecretResolver, name: str, origin: str) -> str | None:
    """Only a secret declared for this origin resolves; anything else is treated as missing."""
    ref = next((r for r in resolver.available() if r.name == name), None)
    if ref is None or not secret_allowed(ref, origin):
        return None
    return await resolver.resolve(name, origin)


class ScopedSecrets:
    """Secret values held in this process, each usable only where it was declared: a run's `SecretResolver`.

    `ScopedSecrets(values, origin)` gives every value the same scope, which is what a run whose secrets all
    belong to its start page needs. `ScopedSecrets.per_secret(...)` scopes each one separately, for a caller
    holding a person's credentials against the sites each of them belongs to.

    An origin may be a `*.` pattern either way, and the same rule decides here as everywhere: a resolver that
    answered on a wider origin than it declared would put the gate in two places with two answers.
    """

    def __init__(self, values: Mapping[str, SecretValue], origin: str) -> None:
        self._secrets: dict[str, tuple[SecretValue, tuple[str, ...]]] = {
            name: (value, (origin,)) for name, value in values.items()
        }

    @classmethod
    def per_secret(cls, secrets: Mapping[str, tuple[SecretValue, Sequence[str]]]) -> "ScopedSecrets":
        """`{name: (value, origins)}`. A secret left with no origins is held by nobody and offered nowhere."""
        scoped = cls({}, "")
        scoped._secrets = {
            name: (value, tuple(origins)) for name, (value, origins) in secrets.items() if tuple(origins)
        }
        return scoped

    def available(self) -> tuple[SecretRef, ...]:
        return tuple(SecretRef(name=name, origins=origins) for name, (_, origins) in self._secrets.items())

    async def resolve(self, name: str, origin: str) -> str | None:
        held = self._secrets.get(name)
        if held is None or not secret_allowed(SecretRef(name=name, origins=held[1]), origin):
            return None
        value = held[0]
        # A computed value is made only once the origin is allowed, and afresh each time it is typed.
        return value if isinstance(value, str) else await value()


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

    def redact_url(self, url: str) -> str:
        """Redact an address everywhere but its host and port, so what is reported is still an address.

        A secret that is an ordinary word (`practice`) also names hosts: redacting it out of
        `https://practice.expandtesting.com/secure` left `https://[secret:username].expandtesting.com/secure`,
        which no parser reads back. A value that sits wholly inside the host was published by the site in its
        own address, so leaving it there tells nobody anything. Userinfo, path, query and fragment are still
        redacted, and a value that runs across the host's edge, or anything that does not parse as an
        address, is redacted as plain text.
        """
        try:
            netloc = urlsplit(url).netloc
        except ValueError:
            return self.redact(url)
        host = netloc.rpartition("@")[2]
        at = url.find(f"//{netloc}")
        if not host or at < 0:
            return self.redact(url)
        end = at + 2 + len(netloc)
        start = end - len(host)
        for value in self._values:
            found = url.find(value)
            while found >= 0:
                if found < end and found + len(value) > start and not start <= found <= end - len(value):
                    return self.redact(url)
                found = url.find(value, found + 1)
        return self.redact(url[:start]) + url[start:end] + self.redact(url[end:])

    def mask(self, text: str) -> str:
        """Blank secret values at equal length, so offsets into the text (capture blocks) stay valid."""
        for value in sorted(self._values, key=len, reverse=True):
            text = text.replace(value, "•" * len(value))
        return text

    def reveals(self, text: str) -> bool:
        return any(value in text for value in self._values)
