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
    ArmBinding("browser-use", "browser-use-cloud", "bu-4.0"),
    ArmBinding("browser-use-ultrafast", "browser-use-cloud", "bu-ultrafast"),
    # Parallax's Browser Use harnesses take the model they run explicitly; bu-4.0 is browser-use 1.0.6's default.
    ArmBinding("browser-use-oss", "browser-use-oss", "bu-4.0"),
    # Codex on Luna, price-matched with the Claude Code arm (browser-harness-claude-code on Haiku 5.5, diagnostic).
    ArmBinding("browser-harness", "browser-harness", "openai/gpt-6-luna"),
    ArmBinding("cua-codex", "cua-codex"),
)

BY_PUBLIC = {binding.public: binding for binding in ARM_BINDINGS}
# Rows recorded before each arm pinned its model: the cloud agent on its project default, browser-use 1.0.5 on bu-fast.
LEGACY: dict[tuple[str, str | None], PublicArm] = {
    ("browser-use-cloud", None): "browser-use",
    ("browser-use-oss", "bu-fast"): "browser-use-oss",
    ("browser-harness", None): "browser-harness",
}
BY_PARALLAX = LEGACY | {(binding.harness, binding.model): binding.public for binding in ARM_BINDINGS}


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
