"""The rules an entry point applies to what its operator asked for, written once for the CLI, the MCP server and
`serve`.

The CLI and the MCP server take the same flags for the browser and for secrets, and both had their own copy of
what those flags mean. `serve` takes the browser ones as fields of a `run` request.
A rule with two copies is a rule that can come to mean two things: the MCP server would keep letting `--headed`
unset `FASTBROWSE_HEADED` long after the CLI stopped, and nothing would fail.
"""

import argparse
import hashlib
import os
import shutil
import tempfile
from collections.abc import Mapping
from pathlib import Path
from urllib.parse import urlsplit

from fastbrowse.clients.environment import ConfigurationError, Settings
from fastbrowse.models import CostBreakdown, LocalChrome, RunResult, SecretValue, Status, StepResult


def cloud(local: bool, chrome: LocalChrome, cloud_profile: str | None, proxy_country: str | None = None) -> bool:
    """Browser Use Cloud unless the operator asked for local Chrome: `--local`, or a headed window or kept profile,
    which only local Chrome has, whether from its flag or from FASTBROWSE_HEADED / FASTBROWSE_PROFILE.

    Cloud is the default: its browsers pass bot checks a fresh local Chrome fails, and they carry none of a desktop
    Chrome's own interface (see `_quiet_password_manager`).

    A cloud profile or a country asked of local Chrome is refused: the run would start signed out, or browse from
    this machine's address, and report neither.
    """
    on_cloud = not (local or chrome.headed or chrome.profile is not None)
    for flag, value in (("--cloud-profile", cloud_profile), ("--proxy-country", proxy_country)):
        if value is not None and not on_cloud:
            raise ConfigurationError(
                f"{flag} needs the cloud browser: drop it, or drop --local, --headed and --profile "
                "(and FASTBROWSE_HEADED / FASTBROWSE_PROFILE)"
            )
    return on_cloud


def handed_over(
    cdp_url: str | None,
    cdp_port: int | None,
    *,
    attach: bool,
    target_match: str | None,
    local: bool,
    cloud_asked: bool = False,
    chrome: LocalChrome,
    cloud_profile: str | None,
    proxy_country: str | None,
) -> bool:
    """Whether the run drives a browser the operator already runs, named by URL or by port.

    Attaching replaces every option that shapes a started browser, so combining them is refused the way
    `--cloud-profile` on local Chrome is: as a configuration error, before any browser work. `--headed` and
    `--profile` count from FASTBROWSE_HEADED / FASTBROWSE_PROFILE too, which `chrome` already reflects.
    """
    if cdp_url is not None and cdp_port is not None:
        raise ConfigurationError("--cdp-url and --cdp-port both name a browser to attach to; pass one")
    if cdp_url is None and cdp_port is None:
        if attach or target_match is not None:
            raise ConfigurationError("--attach and --target-match need --cdp-url or --cdp-port")
        return False
    flag = "--cdp-url" if cdp_url is not None else "--cdp-port"
    conflicts = [
        conflict
        for conflict, on in (
            ("--local", local),
            ("--cloud", cloud_asked),
            ("--headed", chrome.headed),
            ("--profile", chrome.profile is not None),
            ("--cloud-profile", cloud_profile is not None),
            ("--proxy-country", proxy_country is not None),
        )
        if on
    ]
    if conflicts:
        raise ConfigurationError(f"{flag} attaches to a browser already running; drop {', '.join(conflicts)}")
    if cdp_url is not None and not cdp_url.startswith(("ws://", "wss://")):
        raise ConfigurationError(f"--cdp-url expects a ws:// or wss:// URL, got {cdp_url!r}")
    if cdp_port is not None and not 0 < cdp_port < 65536:
        raise ConfigurationError(f"--cdp-port expects a port from 1 to 65535, got {cdp_port}")
    return True


def recording(record: Path | None) -> None:
    """A recording is encoded by ffmpeg, and without it the run would browse to the end and save nothing."""
    if record is not None and shutil.which("ffmpeg") is None:
        raise ConfigurationError("--record needs ffmpeg on PATH")


def country_code(value: str) -> str:
    """A two-letter code, lowercased as Browser Use Cloud expects; a shop serves the visitor's country from it."""
    code = value.strip().lower()
    if len(code) != 2 or not code.isalpha():
        raise argparse.ArgumentTypeError(f"{value!r} is not a two-letter country code, e.g. uk")
    if code == "gb":
        # ISO says gb; Browser Use's code for the United Kingdom is uk, and it would reject gb only at browser start.
        raise argparse.ArgumentTypeError("Browser Use's code for the United Kingdom is uk, not gb")
    return code


def proxy_country(asked: str | None) -> str:
    """The country a cloud browser browses from: the one asked for, and the United States when none was."""
    return "us" if asked is None else asked


def browser_key(settings: Settings, on_cloud: bool) -> str | None:
    """The cloud browser's key when the run wants one; None runs local Chrome."""
    return settings.browser_key() if on_cloud else None


def chrome(settings: Settings, headed: bool, profile: Path | None, binary: str | None = None) -> LocalChrome:
    """The flags add to FASTBROWSE_HEADED and FASTBROWSE_PROFILE; they cannot unset them.

    `binary` replaces FASTBROWSE_CHROME instead: each names one program, and a caller who names one for this run
    means that one.
    """
    local = settings.local_chrome()
    return local.model_copy(
        update={
            "binary": binary or local.binary,
            "headed": headed or local.headed,
            "profile": profile or local.profile,
        }
    )


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


def error_result(error: str) -> RunResult:
    """A run with nothing to report but why it failed, in the shape of any other, so a caller branches on `status`.

    It stands for a run the CLI never started, and for one `serve` saw end on an exception, whose browser has
    closed by then: what that run did is in the events already sent.
    """
    return RunResult(
        status=Status.ERROR,
        answer=None,
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(lines=()),
        artifacts=(),
        error=error,
    )


def step_label(step: StepResult) -> str:
    """One step as a line: what was done, and to what."""
    return f"{step.operation.value} {step.target or ''}".strip()


def keep_screenshot(frame: bytes, directory: Path | None) -> Path:
    """Write a run's final-page image where its caller can open it, and return the path.

    The image travels only in `RunResult.final_frame`, which JSON leaves out, so a CLI or MCP caller that asked for a
    screenshot was told one was captured and never received it. Without a downloads directory it goes to a fresh
    temporary one, since the run's own scratch space is deleted when the run ends.
    """
    root = directory.resolve() if directory is not None else Path(tempfile.mkdtemp(prefix="fastbrowse-"))
    # Content-addressed like downloads, so a second run never overwrites the first one's image.
    path = root / "screenshots" / hashlib.sha256(frame).hexdigest()[:16] / "screenshot.png"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(frame)
    return path
