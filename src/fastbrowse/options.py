"""The rules an entry point applies to what its operator asked for, written once for the CLI and the MCP server.

Both take the same flags for the browser and for secrets, and both had their own copy of what those flags mean.
A rule with two copies is a rule that can come to mean two things: the MCP server would keep letting `--headed`
unset `FASTBROWSE_HEADED` long after the CLI stopped, and nothing would fail.
"""

import argparse
import os
from collections.abc import Mapping
from pathlib import Path
from urllib.parse import urlsplit

from fastbrowse.clients.environment import ConfigurationError, Settings
from fastbrowse.models import LocalChrome, SecretValue, StepResult


def cloud(local: bool, chrome: LocalChrome, cloud_profile: str | None) -> bool:
    """Browser Use Cloud unless the operator asked for local Chrome: `--local`, or a headed window or kept profile,
    which only local Chrome has, whether from its flag or from FASTBROWSE_HEADED / FASTBROWSE_PROFILE.

    Cloud is the default: its browsers pass bot checks a fresh local Chrome fails, and they carry none of a desktop
    Chrome's own interface (see `_quiet_password_manager`).
    """
    on_cloud = not (local or chrome.headed or chrome.profile is not None)
    if cloud_profile is not None and not on_cloud:
        raise ConfigurationError(
            "--cloud-profile needs the cloud browser: drop it, or drop --local, --headed and --profile "
            "(and FASTBROWSE_HEADED / FASTBROWSE_PROFILE)"
        )
    return on_cloud


def country_code(value: str) -> str:
    """A two-letter code, lowercased as Browser Use Cloud expects; a shop serves the visitor's country from it."""
    code = value.strip().lower()
    if len(code) != 2 or not code.isalpha():
        raise argparse.ArgumentTypeError(f"{value!r} is not a two-letter country code, e.g. uk")
    if code == "gb":
        # ISO says gb; Browser Use's code for the United Kingdom is uk, and it would reject gb only at browser start.
        raise argparse.ArgumentTypeError("Browser Use's code for the United Kingdom is uk, not gb")
    return code


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


def scoped_secret(value: str) -> tuple[str, str, str | None]:
    """`NAME=ENV_VAR` or `NAME=ENV_VAR@ORIGIN`, with the origin absent when none was given.

    The name is taken first: a secret may be named for the account it belongs to, and `user@example.com=PW@...`
    has an `@` in its name before the one that introduces the origin.

    Absent means "not stated here", never "any origin": an entry point that has no other source for the scope
    must refuse it. The CLI takes the start origin in that case and the MCP server has none to take, which is
    why the choice belongs to them and the parsing belongs here.
    """
    named, equals, rest = value.partition("=")
    variable, at, origin = rest.partition("@")
    name, variable = env_secret(f"{named}{equals}{variable}")
    if not at:
        return name, variable, None
    parts = urlsplit(origin)
    if parts.scheme not in ("http", "https") or not parts.hostname:
        raise argparse.ArgumentTypeError(f"expected NAME=ENV_VAR@https://host, got {value!r}")
    if parts.path not in ("", "/") or parts.query or parts.fragment:
        raise argparse.ArgumentTypeError(f"{origin!r} is not an origin: drop everything after the host")
    return name, variable, origin


def unset_variables(pairs: list[tuple[str, str, str | None]], environ: Mapping[str, str] = os.environ) -> list[str]:
    """The variables a `--secret` names that the environment does not hold, so a run fails before a browser opens."""
    return sorted({variable for _, variable, _ in pairs if variable not in environ})


def merged_secrets(values: Mapping[str, SecretValue], vault: Mapping[str, SecretValue]) -> dict[str, SecretValue]:
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
