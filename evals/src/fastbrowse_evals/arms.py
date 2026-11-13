"""The public arm projection used by campaign execution and publication."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal, cast

PublicArm = Literal[
    "fastbrowse",
    "fastbrowse-pool",
    "fastbrowse-hosted",
    "browser-use",
    "browser-use-ultrafast",
    "browser-use-oss",
    "browser-harness",
    "cua-codex",
]


@dataclass(frozen=True, slots=True)
class ArmBinding:
    public: PublicArm
    harness: str
    model: str | None = None


ARM_BINDINGS: tuple[ArmBinding, ...] = (
    ArmBinding("fastbrowse", "fastbrowse"),
    ArmBinding("fastbrowse-pool", "fastbrowse-pool"),
    ArmBinding("fastbrowse-hosted", "fastbrowse-hosted"),
    ArmBinding("browser-use", "browser-use-cloud"),
    ArmBinding("browser-use-ultrafast", "browser-use-cloud", "bu-ultrafast"),
    ArmBinding("browser-use-oss", "browser-use-oss"),
    ArmBinding("browser-harness", "browser-harness"),
    ArmBinding("cua-codex", "cua-codex"),
)

BY_PUBLIC = {binding.public: binding for binding in ARM_BINDINGS}
BY_PARALLAX = {(binding.harness, binding.model): binding.public for binding in ARM_BINDINGS}


def binding(public: str) -> ArmBinding:
    try:
        return BY_PUBLIC[cast(PublicArm, public)]
    except KeyError:
        raise ValueError(f"unknown public arm {public!r}") from None


def public_arm(harness: str, model: str | None) -> PublicArm | None:
    return BY_PARALLAX.get((harness, model)) or BY_PARALLAX.get((harness, None))


def invocations(public_arms: tuple[str, ...] | list[str]) -> tuple[ArmBinding, ...]:
    """Return one Parallax invocation per distinct harness and model pair."""
    seen: set[tuple[str, str | None]] = set()
    result: list[ArmBinding] = []
    for name in public_arms:
        item = binding(name)
        key = (item.harness, item.model)
        if key not in seen:
            seen.add(key)
            result.append(item)
    return tuple(result)


def print_invocations(public_arms: list[str]) -> None:
    for item in invocations(public_arms):
        print(f"{item.public}\t{item.harness}\t{item.model or ''}")
