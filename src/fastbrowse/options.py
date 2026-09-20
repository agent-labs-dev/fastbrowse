"""The rules an entry point applies to what its operator asked for, written once for the CLI and the MCP server.

Both take the same flags for the browser and for secrets, and both had their own copy of what those flags mean.
A rule with two copies is a rule that can come to mean two things: the MCP server would keep letting `--headed`
unset `FASTBROWSE_HEADED` long after the CLI stopped, and nothing would fail.
"""

import argparse
import os
from collections.abc import Mapping
from pathlib import Path

from fastbrowse.clients.environment import Settings
from fastbrowse.models import LocalChrome, StepResult


def browser_key(settings: Settings, cloud: bool) -> str | None:
    """The cloud browser's key when the run wants one; None runs local Chrome."""
    return settings.browser_key() if cloud else None


def chrome(settings: Settings, headed: bool, profile: Path | None) -> LocalChrome:
    """The flags add to FASTBROWSE_HEADED and FASTBROWSE_PROFILE; they cannot unset them."""
    local = settings.local_chrome()
    return local.model_copy(update={"headed": headed or local.headed, "profile": profile or local.profile})


def env_secret(pair: str) -> tuple[str, str]:
    """`NAME=ENV_VAR` as an argparse type. The MCP server's `NAME=ENV_VAR@ORIGIN` parses the same first half."""
    name, sep, variable = pair.partition("=")
    if not (sep and name and variable):
        raise argparse.ArgumentTypeError(f"expected NAME=ENV_VAR, got {pair!r}")
    return name, variable


def unset_variables(pairs: list[tuple[str, str]], environ: Mapping[str, str] = os.environ) -> list[str]:
    """The variables a `--secret` names that the environment does not hold, so a run fails before a browser opens."""
    return sorted({variable for _, variable in pairs if variable not in environ})


def merged_secrets(values: Mapping[str, str], vault: Mapping[str, str]) -> dict[str, str]:
    """Declared secrets and a vault item's, refusing a name both supply rather than letting one win silently.

    Raises `ValueError`, which each entry point reports in its own terms: the CLI as a configuration error before
    the run, the MCP server as a tool error on the call that asked for that item.
    """
    if clash := values.keys() & vault.keys():
        raise ValueError(f"a declared secret and the vault item both set {', '.join(sorted(clash))}")
    return {**values, **vault}


def step_label(step: StepResult) -> str:
    """One step as a line: what was done, and to what."""
    return f"{step.operation.value} {step.target or ''}".strip()
