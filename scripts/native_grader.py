#!/usr/bin/env python3
"""A trusted grader command for the external corpus: the pinned upstream state grader plus a strict supplement.

It speaks the `external_grade` JSON stdio bridge. The upstream grader runs unchanged, on the request without the
trusted key below, and its grade is kept whole under `evidence.upstream`. A task with a supplement must also satisfy
that supplement, which is bound to the exact task (dataset pin, site, text) and upstream predicate by SHA-256 and
reported under its own label. A supplement never rewrites an upstream grade. It only adds a requirement, so a pass
needs both.

`before` and `during` readings come only from the top-level request key `native_observations`, written by the
harness's own observer (see `native_state.TrustedObservations`) and accepted only when its `observer_sha256` equals
`--observer-sha256`. Nothing in `outcome`, the agent's answer or its data, is ever read as a witness. A required
witness the observer did not supply leaves the attempt ungraded (exit 3), as does an observer that cannot read and a
supplement that lists a requirement nothing can witness.

    native_grader.py --supplements FILE [--observer-sha256 HEX] [--observe-arg ARG ...] [--base-urls FILE]
                     [--auth-file FILE] --upstream-code FILE --upstream-code-sha256 HEX
                     --upstream COMMAND [ARG ...]
    native_grader.py --supplements FILE --sample-phase {before,during,request} [--observe-arg ARG ...]
                     [--base-urls FILE] [--auth-file FILE]

Grade mode requires the upstream pin as a pair, checked before any outcome is read: `--upstream-code` must be a file
named in the `--upstream` command and `--upstream-code-sha256` its digest. Sample mode has no upstream and needs
neither. `--base-urls` origins must be bare `http(s)://host[:port]`, and the HTTP reader follows no redirect (a
redirect would carry the credential to another origin), so a redirect leaves the attempt ungraded.

`--observe-arg` is a command template for probe observers: `{site}`, `{probe}` and `{args}` (JSON) are
substituted per argument and the command prints JSON. `--base-urls` maps site to origin for HTTP observers and
`--auth-file` holds one `user:password` line for them; the credential is never printed or put in evidence.

`--sample-phase` makes this script the harness's observer instead (`corpus_compare --observer-command`). It reads
`{"task": TaskRef, "phase": "before"|"during"}` on stdin and prints the runner witness for that phase as a mapping
of observer name to reading, with the same probe and HTTP readers and never the upstream grader. `before` and
`during` fix the phase and refuse a request for another; `request` takes it from stdin, so one command serves
both. A task with no supplement prints `{}` and touches nothing. An unreadable observer exits 3 and prints no
reading, so the watcher records an error and no sample.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from pydantic import ValidationError

from fastbrowse.evals.corpus import Grade
from fastbrowse.evals.endpoint_adapter import NoRedirect, read_url_map
from fastbrowse.evals.native_state import (
    Http,
    Observation,
    Probe,
    Supplement,
    TrustedObservations,
    Witness,
    canonical_digest,
    evaluate,
    parse_supplements,
    select_observations,
)

GRADER = "windtunnel-native-state+strict-supplement"
VERSION = "1"
UNGRADED = 3
"""Exit status for an attempt the supplement cannot judge. The bridge reads any nonzero exit as ungraded."""
TIMEOUT = 90
_OPENER = urllib.request.build_opener(NoRedirect)


class Ungraded(Exception):
    """The attempt cannot be judged, which is different from failing it."""


def _fail(message: str, code: int = 2) -> None:
    print(message, file=sys.stderr)
    raise SystemExit(code)


def check_upstream_pin(args: argparse.Namespace) -> None:
    """Raise ValueError unless the upstream command runs a file whose digest is the one pinned."""
    code, digest = args.upstream_code, args.upstream_code_sha256
    if code is None or digest is None:
        raise ValueError("grade mode needs both --upstream-code and --upstream-code-sha256")
    if not re.fullmatch(r"[a-f0-9]{64}", digest):
        raise ValueError("--upstream-code-sha256 must be a lowercase SHA-256 hex digest")
    if not code.is_file():
        raise ValueError(f"--upstream-code is not a file: {code}")
    if not any(Path(part).resolve() == code.resolve() for part in args.upstream if part):
        raise ValueError("--upstream-code must be a file the --upstream command runs")
    actual = hashlib.sha256(code.read_bytes()).hexdigest()
    if actual != digest:
        raise ValueError(f"upstream grader code does not match its pin: {actual} != {digest}")


def run_upstream(command: list[str], payload: bytes, code: Path, code_sha256: str) -> Grade:
    # Rechecked at run time: the file may change between startup and the grade.
    actual = hashlib.sha256(code.read_bytes()).hexdigest()
    if actual != code_sha256:
        _fail(f"upstream grader code changed: {actual} != {code_sha256}")
    try:
        result = subprocess.run(command, input=payload, capture_output=True, timeout=TIMEOUT * 2, check=False)
    except (OSError, subprocess.TimeoutExpired) as exc:
        _fail(f"upstream grader did not run: {exc}")
    if result.returncode != 0:
        _fail(f"upstream grader exited {result.returncode}: {result.stderr.decode(errors='replace')[:400]}")
    return Grade.model_validate_json(result.stdout)


def read_probe(observer: Probe, site: str, template: list[str]) -> object:
    if not template:
        raise Ungraded(f"{observer.name}: no --observe-arg command is configured for probe observers")
    values = {"site": site, "probe": observer.probe, "args": json.dumps(observer.args)}
    command = [part.format(**values) for part in template]
    try:
        result = subprocess.run(command, capture_output=True, timeout=TIMEOUT, check=False)
        return json.loads(result.stdout) if result.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as exc:
        raise Ungraded(f"{observer.name}: probe could not be read: {exc}") from exc


def read_http(observer: Http, site: str, bases: dict[str, str], auth: str | None) -> object:
    base = bases.get(site)
    if base is None:
        raise Ungraded(f"{observer.name}: no origin configured for site")
    query = f"?{urllib.parse.urlencode(observer.query)}" if observer.query else ""
    request = urllib.request.Request(f"{base.rstrip('/')}{observer.path}{query}")
    if auth:
        request.add_header("Authorization", "Basic " + base64.b64encode(auth.encode()).decode())
    try:
        with _OPENER.open(request, timeout=TIMEOUT) as response:
            return json.loads(response.read())
    except (OSError, urllib.error.URLError, ValueError) as exc:
        raise Ungraded(f"{observer.name}: HTTP observer could not be read: {type(exc).__name__}") from exc


def read_witness(witness: Witness, site: str, args: argparse.Namespace, auth: str | None) -> Observation:
    data: dict[str, object] = {}
    for observer in witness.observe:
        if isinstance(observer, Probe):
            reading = read_probe(observer, site, args.observe_arg)
        else:
            reading = read_http(observer, site, args.bases, auth)
        if reading is None:
            raise Ungraded(f"{witness.label}.{observer.name}: observer returned nothing")
        data[observer.name] = reading
    return Observation(label=witness.label, data=data)  # type: ignore[arg-type]


def sample(args: argparse.Namespace, payload: bytes) -> dict[str, object]:
    """The runner witness for the requested phase, read independently of any upstream grade."""
    request = json.loads(payload)
    task, phase = request["task"], request.get("phase")
    if phase not in ("before", "during") or args.sample_phase not in (phase, "request"):
        _fail(f"cannot sample phase {phase!r} with --sample-phase {args.sample_phase}")
    supplement: Supplement | None = args.supplements.get(task["id"])
    if supplement is None:
        return {}
    check_identity(task, supplement)
    auth = args.auth_file.read_text(encoding="utf-8").strip() if args.auth_file else None
    data: dict[str, object] = {}
    for witness in supplement.witnesses:
        if witness.source == "runner" and witness.label == phase:
            reading = read_witness(witness, task.get("site") or "", args, auth).data
            if isinstance(reading, dict):
                data.update(reading)
    return data


def trusted_observations(
    request: dict[str, object], supplement: Supplement, observer_sha256: str | None
) -> list[Observation]:
    """The observer's readings for this task, or nothing when there are none or they are not from the pinned observer.

    The key is the harness's, beside `outcome` and never inside it, because `outcome` is what the agent produced."""
    raw = request.get("native_observations")
    if raw is None:
        return []
    try:
        trusted = TrustedObservations.model_validate(raw)
    except ValidationError as exc:
        raise Ungraded(f"{supplement.id}: native_observations is malformed: {exc.error_count()} errors") from exc
    if observer_sha256 is None or trusted.observer_sha256 != observer_sha256:
        raise Ungraded(f"{supplement.id}: native_observations did not come from the pinned observer")
    if trusted.task_id != supplement.task_id:
        raise Ungraded(f"{supplement.id}: native_observations belong to task {trusted.task_id}")
    return select_observations(supplement, trusted)


def check_identity(task: dict[str, object], supplement: Supplement) -> str:
    """The upstream predicate digest, after the task is shown to be the exact one the supplement was written for."""
    metadata = task.get("metadata")
    predicate = metadata.get("predicate") if isinstance(metadata, dict) else None
    digest = canonical_digest(predicate)
    text = task.get("task")
    pinned = {
        "source": (task.get("source"), supplement.source),
        "revision": (task.get("revision"), supplement.revision),
        "dataset sha256": (task.get("sha256"), supplement.dataset_sha256),
        "site": (task.get("site"), supplement.site),
        "task text sha256": (
            hashlib.sha256(text.encode()).hexdigest() if isinstance(text, str) else None,
            supplement.task_sha256,
        ),
        "upstream predicate sha256": (digest, supplement.upstream_sha256),
    }
    for name, (actual, expected) in pinned.items():
        if actual != expected:
            _fail(f"{supplement.task_id}: supplement pins {name} {expected}, task carries {actual}")
    return digest


def grade(args: argparse.Namespace, payload: bytes) -> Grade:
    request = json.loads(payload)
    task = request["task"]
    # The upstream grader is the pinned original and sees exactly the request shape it was written for.
    forwarded = json.dumps({key: value for key, value in request.items() if key != "native_observations"}).encode()
    upstream = run_upstream(args.upstream, forwarded, args.upstream_code, args.upstream_code_sha256)
    supplement: Supplement | None = args.supplements.get(task["id"])
    if supplement is None:
        return upstream
    digest = check_identity(task, supplement)
    supplied = {item.label: item for item in trusted_observations(request, supplement, args.observer_sha256)}
    auth = args.auth_file.read_text(encoding="utf-8").strip() if args.auth_file else None
    observations: list[Observation] = []
    for witness in supplement.witnesses:
        if witness.source == "grade":
            observations.append(read_witness(witness, task.get("site") or "", args, auth))
        elif witness.label in supplied:
            observations.append(supplied[witness.label])
    verdict = evaluate(supplement, observations)
    if upstream.passed and verdict.status == "ungraded":
        raise Ungraded(f"{supplement.id}: required witness not supplied: {', '.join(verdict.missing)}")
    if upstream.passed and verdict.passed and supplement.unwitnessed:
        raise Ungraded(f"{supplement.id}: limited, cannot witness: {'; '.join(supplement.unwitnessed)}")
    passed = upstream.passed and verdict.passed
    failure = upstream.failure if not upstream.passed else "; ".join(verdict.failures) or None
    return Grade(
        grader=GRADER,
        # The content digest means a changed check is a different grading identity even when `version` was not bumped.
        version=(
            f"{VERSION}+{upstream.grader}.{upstream.version}"
            f"+{supplement.id}.{supplement.version}.{canonical_digest(supplement.model_dump(mode='json'))[:16]}"
        ),
        passed=passed,
        failure=None if passed else failure,
        evidence={
            "upstream": {
                "label": "upstream",
                **upstream.model_dump(mode="json"),
                "predicate_sha256": digest,
            },
            "strict_supplement": {
                "label": "strict-supplement",
                "id": supplement.id,
                "version": supplement.version,
                "status": verdict.status,
                "failures": list(verdict.failures),
                "missing": list(verdict.missing),
                "checked": list(verdict.checked),
                "unwitnessed": list(supplement.unwitnessed),
                "observations": [
                    item.model_dump(mode="json") for item in observations if item.label in verdict.checked
                ],
            },
        },
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--supplements", required=True, type=Path)
    parser.add_argument("--observer-sha256", help="digest of the harness observer whose samples are accepted")
    parser.add_argument("--observe-arg", action="append", default=[])
    parser.add_argument("--base-urls", type=Path)
    parser.add_argument("--auth-file", type=Path)
    parser.add_argument("--upstream-code", type=Path)
    parser.add_argument("--upstream-code-sha256")
    parser.add_argument("--sample-phase", choices=("before", "during", "request"))
    parser.add_argument("--upstream", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if not args.upstream and args.sample_phase is None:
        parser.error("--upstream needs a command")
    try:
        args.supplements = parse_supplements(json.loads(args.supplements.read_text(encoding="utf-8")))
        if args.observer_sha256 is not None and not re.fullmatch(r"[a-f0-9]{64}", args.observer_sha256):
            raise ValueError("--observer-sha256 must be a SHA-256 hex digest")
        args.bases = read_url_map(args.base_urls) if args.base_urls else {}
        if args.sample_phase is None:
            check_upstream_pin(args)
    except (OSError, ValueError) as exc:
        _fail(f"cannot read configuration: {exc}")
    try:
        if args.sample_phase is not None:
            sys.stdout.write(json.dumps(sample(args, sys.stdin.buffer.read())))
            return 0
        result = grade(args, sys.stdin.buffer.read())
    except Ungraded as exc:
        print(f"ungraded: {exc}", file=sys.stderr)
        return UNGRADED
    sys.stdout.write(result.model_dump_json())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
