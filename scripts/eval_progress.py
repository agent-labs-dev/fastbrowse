"""Publish a running eval's progress as a commit status, since a job's logs stay unreadable until it ends.

    GH_TOKEN=... uv run python scripts/eval_progress.py runs/bu-bench bu-bench [--final]

The target is a campaign ledger or an official run's folder. It runs beside the suite, not inside it, so the suite's
step never holds the token. Read it back with `gh api repos/OWNER/REPO/commits/SHA/status`.
"""

import argparse
import contextlib
import json
import os
import pathlib
import signal
import tempfile
import time
import urllib.request

INTERVAL_SECONDS = 60


def pid_file() -> pathlib.Path:
    """Where the reporting loop leaves its process id: the job's own temporary directory, one loop per job."""
    return pathlib.Path(os.environ.get("RUNNER_TEMP") or tempfile.gettempdir()) / "eval-progress.pid"


def stop_loop() -> None:
    """Stop the job's reporting loop, so a pending report cannot land after the final one and leave it pending."""
    path = pid_file()
    with contextlib.suppress(OSError, ValueError):
        os.kill(int(path.read_text()), signal.SIGTERM)
    path.unlink(missing_ok=True)


def rows(path: pathlib.Path) -> list[dict]:
    """A ledger's rows, leaving out a line the suite is still writing."""
    found = []
    for line in path.read_text().splitlines():
        try:
            found.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return found


def summary(target: pathlib.Path) -> str:
    """One line on what has finished so far, short enough for a commit status."""
    if target.is_dir():
        done = list(target.glob("**/agent.json"))
        statuses = [json.loads(path.read_text()).get("status") for path in done]
        return f"{len(done)} attempts, {statuses.count('complete')} complete"
    if not target.is_file():
        return "starting"
    ledger = rows(target)
    passed = sum(1 for row in ledger if row.get("passed"))
    failed = sum(1 for row in ledger if row.get("passed") is False)
    dollars = sum(row.get("dollars") or 0 for row in ledger)
    last = ledger[-1].get("task") if ledger else None
    return f"{len(ledger)} rows, {passed} passed, {failed} failed, ${dollars:.2f}, last {last}"


def publish(label: str, state: str, description: str) -> None:
    repository, sha = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_SHA"]
    body = {"state": state, "context": f"eval progress / {label}", "description": description[:140]}
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repository}/statuses/{sha}",
        json.dumps(body).encode(),
        {"Authorization": f"Bearer {os.environ['GH_TOKEN']}", "Accept": "application/vnd.github+json"},
    )
    urllib.request.urlopen(request, timeout=30).close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", type=pathlib.Path)
    parser.add_argument("label")
    parser.add_argument("--final", action="store_true", help="publish once and mark the group finished")
    args = parser.parse_args()
    if args.final:
        stop_loop()
        publish(args.label, "success", summary(args.target))
        return
    pid_file().write_text(str(os.getpid()))
    while True:
        # A failed report must not stop later ones, and it never touches the suite.
        try:
            publish(args.label, "pending", summary(args.target))
        except (OSError, ValueError) as error:
            print(f"progress not published: {error}")
        time.sleep(INTERVAL_SECONDS)


if __name__ == "__main__":
    main()
