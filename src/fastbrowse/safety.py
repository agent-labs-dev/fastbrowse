"""Where a secret may be typed, by the rule the run applies before it asks a `SecretResolver` for a value.

The run checks every secret against its declared origins itself; these are for an embedder that has to answer the
same question outside a run, such as which saved login covers a page, and a resolver that holds its values here.
"""

import importlib
from collections.abc import Awaitable, Callable, Mapping, Sequence
from typing import Any

from fastbrowse.models import SecretRef, SecretResolver

# Compiled, so the checker has no types for it.
_core: Any = importlib.import_module("fastbrowse._native")

type SecretValue = str | Callable[[], Awaitable[str]]
"""A value, or a coroutine function that makes it at the moment it is typed (a one-time code)."""


def origin_of(url: str) -> str:
    """The origin a secret is declared for and typed on, with a port the scheme implies dropped.

    Unlike `fastbrowse.origins.origin_of` it keeps a `*.` pattern (`https://*.example.com`) and always answers: an
    address with no host comes back as `scheme://`, which no secret is declared for.
    """
    return _core.secret_origin(url)


def secret_allowed(ref: SecretRef, origin: str) -> bool:
    """Whether this secret may be typed on `origin`: an exact declared origin, or a `*.` one covering whole labels."""
    return _core.secret_allowed(list(ref.origins), origin)


async def resolve_secret(resolver: SecretResolver, name: str, origin: str) -> str | None:
    """Only a secret declared for this origin resolves; anything else is treated as missing."""
    ref = next((r for r in resolver.available() if r.name == name), None)
    if ref is None or not secret_allowed(ref, origin):
        return None
    return await resolver.resolve(name, origin)


class ScopedSecrets:
    """Secret values held in this process, each usable only where it was declared: a run's `SecretResolver`.

    `ScopedSecrets(values, origin)` gives every value the same scope; `ScopedSecrets.per_secret(...)` scopes each
    one separately, for a caller holding a person's credentials against the sites each belongs to.
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
