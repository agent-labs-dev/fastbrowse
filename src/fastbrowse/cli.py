"""Run one task from the terminal.

    fastbrowse "What is the latest version of httpx?" --start https://pypi.org/
    fastbrowse "Send the form" --start https://example.com/contact --authorize --json

A Browser Use Cloud browser by default (BROWSER_USE_API_KEY), with a URL printed to watch it live;
`--cloud-profile ID` starts it with the cookies that profile holds, and `--proxy-country CC` browsing from that
country. `--local` runs a headless local Chrome instead; `--headed` shows it, and `--profile DIR` keeps its profile
so a site signed into there once stays signed in. Either of those implies `--local`. `--cdp-url ws://…` instead
attaches to a browser already running anywhere, opening one tab and leaving the browser as it was found.
Secrets come from `--secret NAME=ENV_VAR`, read from that variable, or `--bitwarden ITEM`, a vault login's
`username` and `password`, and its `one_time_code` when the item holds an authenticator key.
`--secret NAME=ENV_VAR@ORIGIN` declares an exact or wildcard origin; without it, the scope is the `--start`
origin. A secret with neither is refused. Bitwarden matches the item against `--start` and limits its values to
that origin.

`fastbrowse serve --stdio` is a different program on the same command: it serves runs to another process over
JSON-RPC, and is `serve.py`.
"""

import argparse
import asyncio
import logging
import os
import sys
from importlib.metadata import version
from pathlib import Path

from pydantic import ValidationError

from fastbrowse import options, serve
from fastbrowse.adapters.bitwarden import BitwardenError, bitwarden_login
from fastbrowse.clients.environment import ConfigurationError, load_settings
from fastbrowse.models import (
    Authorization,
    BrowserEvent,
    Limits,
    LocalChrome,
    SecretValue,
    Status,
    StepEvent,
)
from fastbrowse.run import run_task
from fastbrowse.safety import ScopedSecrets, origin_of

EXIT_CODES: dict[Status, int] = {
    Status.COMPLETE: 0,
    Status.ERROR: 1,
    Status.NEEDS_CONFIRMATION: 3,
    Status.NEEDS_LOGIN: 4,
    Status.NEEDS_INPUT: 5,
    Status.BLOCKED: 6,
    Status.BUDGET_EXCEEDED: 7,
    Status.UNAVAILABLE: 8,
    Status.STUCK: 9,
    Status.UNVERIFIED: 10,
    Status.OBSERVATION_LIMIT: 11,
}


def _secrets(
    pairs: list[tuple[str, str, str | None]], bitwarden: str | None, start: str | None
) -> ScopedSecrets | None:
    """Values read now, so a missing one fails before a browser is opened.

    The scope is never taken from wherever the run has got to. A credential that followed the browser would
    be typed on any origin a redirect, an ad frame or a link on a compromised page led to, which is the whole
    reason the allow-list is fixed before the browser opens; the check at dispatch is against the FIELD's
    origin, so both halves have to be right. `NAME=ENV_VAR@https://host` states the scope per secret and needs
    no `--start`; a secret that states none takes the start origin, and one with neither is refused.

    `--bitwarden` still needs `--start`: the vault item is matched BY origin, so there is nothing to match on.
    """
    problems = []
    if missing := options.unset_variables(pairs):
        problems.append(f"--secret names unset variables: {', '.join(missing)}")
    unscoped = [name for name, _, origin in pairs if origin is None]
    if start is None:
        if unscoped:
            problems.append(
                f"--secret {', '.join(unscoped)} needs an origin: give --start, or NAME=ENV_VAR@https://host"
            )
        if bitwarden is not None:
            problems.append("--bitwarden needs --start: the vault item is matched against its origin")
    if problems:
        raise ConfigurationError("; ".join(problems))
    fallback = origin_of(start) if start is not None else None
    scoped: dict[str, tuple[SecretValue, tuple[str, ...]]] = {}
    for name, variable, origin in pairs:
        where = origin or fallback
        # Unreachable while the guard above stands, and it is here so that relaxing that guard cannot silently
        # produce a secret with no scope, which `per_secret` would then hold for nobody rather than refuse.
        if where is None:
            raise ConfigurationError(f"--secret {name} has no origin to be typed on")
        scoped[name] = (os.environ[variable], (where,))
    if bitwarden is not None and start is not None:
        try:
            vault = bitwarden_login(bitwarden, origin_of(start))
        except BitwardenError as exc:
            raise ConfigurationError(str(exc)) from None
        try:
            options.merged_secrets({name: value for name, (value, _) in scoped.items()}, vault)
        except ValueError as exc:
            raise ConfigurationError(str(exc)) from None
        scoped |= {name: (value, (origin_of(start),)) for name, value in vault.items()}
    return ScopedSecrets.per_secret(scoped) if scoped else None


def _parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="fastbrowse",
        description="Run one browser task.",
        epilog="Exit codes: 2 usage error; "
        + "; ".join(f"{code} {status.value}" for status, code in EXIT_CODES.items()),
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {version('fastbrowse')}")
    parser.add_argument("task")
    parser.add_argument(
        "--start", default=None, help="URL to open before the task starts; worked out from the task if omitted"
    )
    browser = parser.add_mutually_exclusive_group()
    browser.add_argument("--cloud", action="store_true", help="use cloud Chrome, overriding local environment defaults")
    browser.add_argument("--local", action="store_true", help="use local Chrome instead of a Browser Use Cloud browser")
    parser.add_argument("--headed", action="store_true", help="show the local Chrome window (implies --local)")
    parser.add_argument(
        "--profile", type=Path, default=None, help="Chrome profile directory kept between runs (implies --local)"
    )
    parser.add_argument(
        "--cloud-profile", metavar="ID", default=None, help="a Browser Use Cloud profile to run signed in as"
    )
    parser.add_argument(
        "--cdp-url",
        metavar="URL",
        default=None,
        help="attach to a browser already running at this CDP websocket URL, instead of starting one",
    )
    parser.add_argument(
        "--cdp-port",
        type=int,
        metavar="PORT",
        default=None,
        help="attach to a browser or Electron app on this DevTools port (http://127.0.0.1:<port>/json/version)",
    )
    parser.add_argument(
        "--attach",
        action="store_true",
        help="attach to an existing page/window without creating a new tab or closing it on exit",
    )
    parser.add_argument(
        "--target-match",
        metavar="PATTERN",
        default=None,
        help="substring match on page title or URL when attaching to an existing window (implies --attach)",
    )
    parser.add_argument(
        "--proxy-country",
        metavar="CC",
        default=None,
        type=options.country_code,
        help="country the cloud browser browses from, as Browser Use's two-letter code, e.g. uk (default: us)",
    )
    parser.add_argument("--authorize", action="store_true", help="allow irreversible actions without confirmation")
    parser.add_argument(
        "--secret",
        action="append",
        default=[],
        type=options.scoped_secret,
        metavar="NAME=ENV_VAR[@ORIGIN]",
    )
    parser.add_argument(
        "--bitwarden",
        metavar="ITEM",
        help="type this vault login's username, password and authenticator code (unlocked bw CLI)",
    )
    parser.add_argument("--max-steps", type=int, default=60)
    parser.add_argument("--max-dollars", type=float, default=None)
    parser.add_argument("--downloads", type=Path, default=None, help="directory for downloaded files")
    parser.add_argument("--json", action="store_true", help="print the full result as JSON")
    parser.add_argument(
        "--record", type=Path, metavar="FILE", help="save a video of the run, ending on its answer (needs ffmpeg)"
    )
    return parser.parse_args(argv)


async def _print_step(event: StepEvent | BrowserEvent) -> None:
    if isinstance(event, BrowserEvent):
        if event.live_url:
            print(f"  watch live: {event.live_url}", file=sys.stderr)
        return
    step = event.step
    print(f"  {step.index:>2} {options.step_label(step)} -> {step.outcome.value}", file=sys.stderr)


def _limits(args: argparse.Namespace) -> Limits:
    """The run's limits, with a rejected one refused the way every other bad flag is.

    `argparse` accepts `--max-steps 0`, and the model that rejects it raises a validation error rather than a
    configuration one, which would have escaped as a traceback and left `--json` with nothing on stdout.
    """
    try:
        return Limits(max_steps=args.max_steps, max_dollars=args.max_dollars)
    except ValidationError as exc:
        bad = ", ".join(f"--{str(error['loc'][0]).replace('_', '-')}" for error in exc.errors() if error["loc"])
        raise ConfigurationError(f"{bad or 'a limit'} must be greater than zero") from None


async def run(args: argparse.Namespace) -> int:
    options.recording(args.record)
    limits = _limits(args)
    # The operator's own flags are checked before the browser key: with the cloud browser the default, a missing key
    # would otherwise hide a secret that could never be typed anywhere.
    secrets = _secrets(args.secret, args.bitwarden, args.start)
    settings = load_settings()
    if args.cloud and (args.headed or args.profile is not None):
        raise ConfigurationError("--cloud cannot be combined with --headed or --profile")
    chrome = LocalChrome() if args.cloud else options.chrome(settings, args.headed, args.profile)
    handed_over = options.handed_over(
        args.cdp_url,
        args.cdp_port,
        attach=args.attach,
        target_match=args.target_match,
        local=args.local,
        cloud_asked=args.cloud,
        chrome=chrome,
        cloud_profile=args.cloud_profile,
        proxy_country=args.proxy_country,
    )
    result = await run_task(
        args.task,
        start=args.start,
        browser_api_key=None
        if handed_over
        else options.browser_key(settings, options.cloud(args.local, chrome, args.cloud_profile, args.proxy_country)),
        chrome=chrome,
        cloud_profile=args.cloud_profile,
        cdp_url=args.cdp_url,
        cdp_port=args.cdp_port,
        attach=args.attach,
        target_match=args.target_match,
        proxy_country=options.proxy_country(args.proxy_country),
        secrets=secrets,
        limits=limits,
        authorization=Authorization(irreversible_actions=args.authorize),
        downloads=args.downloads,
        on_event=_print_step,
        record=args.record,
    )
    if result.status is Status.NEEDS_LOGIN:
        error = (
            f"{result.error or 'Sign-in credentials required'}. Pass --secret NAME=ENV_VAR@https://host "
            "with the password in that environment variable; credentials in task text are not used."
        )
        result = result.model_copy(update={"error": error})
    if args.json:
        print(result.model_dump_json(indent=2))
    else:
        print(f"{result.status.value} (${result.cost.known_dollars:.4f}, {len(result.steps)} steps)")
        # A run that stopped short can carry both: why it stopped, and what it had read by then.
        print("\n\n".join(part for part in (result.error, result.answer) if part))
    if result.final_frame is not None:
        print(f"  screenshot: {options.keep_screenshot(result.final_frame, args.downloads)}", file=sys.stderr)
    if args.record is not None:
        print(
            f"  recorded: {', '.join(map(str, result.recordings))}" if result.recordings else "  not recorded",
            file=sys.stderr,
        )
    return EXIT_CODES[result.status]


def main() -> None:
    # Only as the first argument, so a task that mentions the word, or `--local serve`, is still a task.
    if sys.argv[1:2] == ["serve"]:
        sys.exit(serve.main(sys.argv[2:]))
    args = _parse(sys.argv[1:])
    # Retries, failovers and a recording that could not be written are warnings; say whose they are.
    logging.basicConfig(stream=sys.stderr, level=logging.WARNING, format="fastbrowse: %(levelname)s %(message)s")
    try:
        sys.exit(asyncio.run(run(args)))
    except ConfigurationError as exc:
        if args.json:
            print(options.error_result(str(exc)).model_dump_json(indent=2))
        sys.exit(f"fastbrowse: {exc}")
