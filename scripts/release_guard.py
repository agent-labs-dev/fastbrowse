"""Refuse a release without the fixture suites green on the build it ships.

Every minor or major release needs the fixture suites green on its exact commit. A patch release (`x.y.z` with `z`
above 0) that publishes no comparison skips them: it carries a bug fix that has to reach users within the hour, not
after a half-hour eval campaign, and the scheduled nightly run still covers its commit. The release workflow runs
them itself on the tag and says so with `--evals-ran-here`; a guard run anywhere else looks for a green Evals
workflow run on that commit instead. A release that commits `docs/results/<version>.jsonl` claims a measured
comparison and needs more: the measured build must be in its history and run byte-identical executable code. This
runs before the tag creates the GitHub release or PyPI publishes the wheel, so agent code cannot ship on a red
fixture gate and a comparison cannot be shipped on code nobody ran.

The comparison's evidence commit may follow the clean build it measures, but only documentation may: the
measured commit must be an ancestor of the tag and every tracked file that runs, builds or measures the evals
must be byte-identical between the two. A README or changelog commit cannot invalidate a measurement, and a
`src/`, workflow, lockfile or test change can. The green Evals run is only meaningful because the workflow
refuses a dispatch under three repeats and judges every fixture task at 100% before it spends.

The GitHub queries are read-only and need no repository secret.

    GH_TOKEN=... uv run python scripts/release_guard.py 0.5.19
    uv run python scripts/release_guard.py 0.5.19 --evals-ran-here   # this workflow already ran them
"""

import argparse
import json
import os
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

from fastbrowse.evals import baseline

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = "evals.yml"
API = "https://api.github.com"
DEFAULT_REPOSITORY = "agent-labs-dev/fastbrowse"
DEFAULT_BRANCH = "main"

# Only evidence may follow a measurement. Everything else - source, tests, workflows, lockfiles, browser code,
# package manifests - has to be identical, because a later change can invalidate what the measured build showed.
_EVIDENCE_FILES = {"README.md", "CHANGELOG.md", "AGENTS.md", "CONTRIBUTING.md", "CLAUDE.md"}
_EVIDENCE_PREFIXES = ("docs/", ".agents/")
_HEX40 = re.compile(r"^[0-9a-f]{40}$")
_PATCH = re.compile(r"^\d+\.\d+\.[1-9]\d*$")


def comparison_rows(version: str, root: Path = ROOT) -> list[dict[str, Any]] | None:
    """The comparison a release would publish, or None when it publishes none."""
    return next((rows for release, rows in baseline.published(root / "docs/results") if release == version), None)


def _git(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, check=False)


def _resolve_commit(root: Path, sha: str) -> str | None:
    """The commit a full 40-hex sha names, or None when it is not a commit this checkout holds.

    `--end-of-options` stops a sha that starts with `-` from being read as a git option, and `^{commit}` peels a
    tag or refuses a tree or blob, so only a commit is ever compared against the release.
    """
    if not _HEX40.fullmatch(sha):
        return None
    done = _git(root, "rev-parse", "--verify", "--end-of-options", f"{sha}^{{commit}}")
    return done.stdout.strip() if done.returncode == 0 else None


def in_release(root: Path, sha: str, head: str) -> bool:
    """Whether `sha` is `head` or an ancestor, so a build the tag contains can have measured the comparison."""
    if _resolve_commit(root, sha) != sha:
        return False
    return _git(root, "merge-base", "--is-ancestor", "--end-of-options", sha, head).returncode == 0


def _evidence_only(path: str) -> bool:
    """Whether a path between the measured build and the tag is documentation rather than something that runs."""
    return path in _EVIDENCE_FILES or path.startswith(_EVIDENCE_PREFIXES)


def code_differences(root: Path, measured: str, head: str) -> list[str]:
    """The tracked files that differ between `measured` and `head` and are not evidence-only.

    An empty list means the tag runs the same source, tests, lockfiles and workflows the measurement ran, so a
    green run or a fixture result from `measured` still describes `head`. A git failure is returned as one
    synthetic path, so a comparison that cannot be read is refused rather than assumed identical. Rename detection
    is off: a source file moved into `docs/` would otherwise be reported only by its new evidence path, hiding
    that executable code left its old one.
    """
    done = _git(root, "diff", "--no-renames", "--name-only", "--end-of-options", measured, head)
    if done.returncode != 0:
        return [f"<cannot compare {measured[:12]} with {head[:12]}: {done.stderr.strip()}>"]
    return [line.strip() for line in done.stdout.splitlines() if line.strip() and not _evidence_only(line.strip())]


def _evals_green(repository: str, sha: str, token: str, branch: str = DEFAULT_BRANCH) -> bool:
    # The query names an exact commit, so a ref name or abbreviation would ask about a build the release is not.
    if not _HEX40.fullmatch(sha):
        raise RuntimeError(f"cannot read {WORKFLOW} runs from GitHub: {sha!r} is not a full 40-hex commit sha")
    # The workflow runs on the default branch, so a green dispatch on a tag or a feature branch does not count.
    query = urllib.parse.urlencode({"head_sha": sha, "status": "success", "branch": branch, "per_page": 1})
    request = urllib.request.Request(
        f"{API}/repos/{repository}/actions/workflows/{WORKFLOW}/runs?{query}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "fastbrowse-release-guard",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.load(response)
    except (urllib.error.URLError, TimeoutError, ValueError) as exc:
        raise RuntimeError(f"cannot read {WORKFLOW} runs from GitHub: {exc}") from exc
    if not isinstance(payload, dict) or not isinstance(payload.get("workflow_runs"), list):
        raise RuntimeError(f"cannot read {WORKFLOW} runs from GitHub: unexpected response shape")
    return any(
        isinstance(run, dict)
        and run.get("head_sha") == sha
        and run.get("head_branch") == branch
        and run.get("status") == "completed"
        and run.get("conclusion") == "success"
        for run in payload["workflow_runs"]
    )


def _publication_problems(version: str, root: Path = ROOT) -> list[str]:
    """The publication gate's blocking findings for this release's rows and ledger.

    The release is the last place the files as published are checked, so the same gate that `--publish` and the
    PR diff check run is reused here with the ledger required: conditions that held when the results were
    committed must still hold when the release is tagged.
    """
    from fastbrowse.evals import publication

    target = root / "docs" / "results" / f"{version}.jsonl"
    if not target.exists():
        return []
    rows = [json.loads(line) for line in target.read_text(encoding="utf-8").splitlines() if line.strip()]
    ledger_file = root / "docs" / "results" / f"{version}.attempts.jsonl"
    ledger = publication.read_jsonl(ledger_file) if ledger_file.exists() else None
    baselines_ = [
        row
        for row in publication.baselines(root / "docs" / "results")
        if (row.get("run") or {}).get("fastbrowse_version") != version
    ]
    report = publication.gate(
        rows, release=version, ledger=ledger, baselines_=baselines_, require_ledger=True, require_coverage=True
    )
    return [f"publication {finding.check}: {finding.detail}" for finding in report.blocking]


publication_problems = _publication_problems


def evals_required(version: str, root: Path = ROOT) -> bool:
    """Whether this release must have the fixture suites green on its build before it publishes.

    Only a plain patch version is exempt, so a pre-release or a malformed version fails closed, and a patch that
    publishes a comparison still needs the run its numbers claim.
    """
    return not _PATCH.fullmatch(version) or comparison_rows(version, root) is not None


def problems(
    version: str,
    head: str,
    repository: str,
    token: str,
    root: Path = ROOT,
    *,
    evals_ran_here: bool = False,
) -> list[str]:
    """Every reason this release may not be published; empty when it may."""
    issues: list[str] = []
    # Every minor or major release, not only one that publishes a comparison, needs the Evals workflow green on
    # its exact commit; a release with no results file once skipped this and could ship agent code the fixture
    # gate had not passed. A malformed build fails closed rather than querying a commit that is not a full sha.
    if not _HEX40.fullmatch(head):
        return [f"release build {head!r} is not a full 40-hex commit sha"]
    # The release workflow runs the suites in a job this guard waits on, so its green result is the fixture
    # evidence; asking GitHub for a separately dispatched run would refuse a tag that ran them itself.
    if evals_required(version, root) and not evals_ran_here and not _evals_green(repository, head, token):
        issues.append(f"no successful {WORKFLOW} run on the release build {head[:12]}")
    rows = comparison_rows(version, root)
    if rows is None:
        return issues
    runs = [(row.get("run") or {}) for row in rows]
    dirty = sorted({str(run.get("git_sha")) for run in runs if run.get("git_dirty") is not False})
    if dirty:
        issues.append(f"comparison rows come from an unclean or unrecorded build: {', '.join(dirty)}")
    measured = sorted({str(run.get("git_sha")) for run in runs if run.get("git_sha")})
    if not measured:
        issues.append("comparison rows name no measured build")
    for sha in measured:
        # A ref name, an option or an abbreviation would pass `merge-base` and an empty diff, claiming a measured
        # build that was never pinned. Only a full commit sha can name the build and be compared.
        if not _HEX40.fullmatch(sha):
            issues.append(f"measured build {sha!r} is not a full 40-hex commit sha")
            continue
        if not in_release(root, sha, head):
            issues.append(f"measured build {sha[:12]} is not in the release's history")
            continue
        differing = code_differences(root, sha, head)
        if differing:
            shown = ", ".join(differing[:5]) + ("..." if len(differing) > 5 else "")
            issues.append(
                f"measured build {sha[:12]} ran different executable code than the release build {head[:12]}: {shown}"
            )
    issues += publication_problems(version, root)
    return issues


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="check that a release may be published")
    parser.add_argument("version", nargs="?", help="the release version, e.g. 0.5.19")
    parser.add_argument(
        "--head", default=os.environ.get("GITHUB_SHA"), help="the release commit; defaults to GITHUB_SHA"
    )
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", DEFAULT_REPOSITORY))
    parser.add_argument("--token", default=os.environ.get("GH_TOKEN"), help="a read-only token; defaults to GH_TOKEN")
    parser.add_argument(
        "--evals-ran-here",
        action="store_true",
        help="the fixture suites ran green on this build in this same workflow run, so no earlier run is looked for",
    )
    parser.add_argument(
        "--evals-required",
        action="store_true",
        help="print whether this release needs the fixture suites (true or false) and check nothing else",
    )
    args = parser.parse_args(argv)
    if not args.version:
        parser.error("a version is required")
    required = evals_required(args.version)
    if args.evals_required:
        print("true" if required else "false")
        return 0
    # The commit is needed even with no results, and a token whenever green fixture suites are required and have
    # to be looked up rather than reported by the job that just passed them.
    if not args.head:
        print("no release commit: run from the release workflow or pass --head")
        return 1
    if required and not args.evals_ran_here and not args.token:
        print("no GitHub token: set GH_TOKEN to read the workflow runs")
        return 1
    try:
        issues = problems(
            args.version,
            args.head,
            args.repository,
            args.token,
            evals_ran_here=args.evals_ran_here,
        )
    except RuntimeError as exc:
        print(str(exc))
        return 1
    if issues:
        print("\n".join(issues))
        print(f"REFUSED the release of {args.version}")
        return 1
    if not required:
        evidence = "is a patch release, which skips the fixture suites,"
    elif args.evals_ran_here:
        evidence = "ran the fixture suites green here"
    else:
        evidence = f"has a successful {WORKFLOW} run"
    if comparison_rows(args.version) is None:
        print(f"PASS {args.version} {evidence} on the release build")
        return 0
    print(f"PASS {args.version} {evidence} on code in its history")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
