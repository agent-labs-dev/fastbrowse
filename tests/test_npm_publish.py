"""`scripts/npm_publish.py`: the shape of the 0.9.1 failure, and the runs that must not repeat it.

The fixture is a stand-in `npm` on PATH holding a small registry in a JSON file: it records what was
published, answers `npm view` from that, and can refuse one package the way the real registry refused
`@fastbrowse/linux-arm64@0.9.1` on 2026-10-30 (`TLOG_CREATE_ENTRY_ERROR`, 409).
"""

import json
import os
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts" / "npm_publish.py"
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")
VERSION = "0.9.1"

FAKE_NPM = '''\
#!{python}
"""A registry in a JSON file, enough of npm's CLI for the publish script."""
import json, os, sys
from pathlib import Path

state_path = Path(os.environ["FAKE_NPM_STATE"])
state = json.loads(state_path.read_text())
version = state["version"]
args = sys.argv[1:]


def save():
    state_path.write_text(json.dumps(state))


def name_of(path):
    directory = Path(path).name
    return "fastbrowse" if directory == "sdk" else f"@fastbrowse/{{directory}}"


if args[0] == "view":
    field = args[2] if len(args) > 2 else ""
    if field == "dist-tags.latest":
        if state["lag"].get(args[1], 0) > 0:
            # The registry's cache still serves the document from before the publish.
            state["lag"][args[1]] -= 1
            save()
            print("0.9.0")
            sys.exit(0)
        if args[1] not in state["latest"]:
            sys.exit(1)
        print(state["latest"][args[1]])
        sys.exit(0)
    name, _, asked = args[1].rpartition("@")
    if f"{{name}}@{{asked}}" not in state["published"]:
        sys.exit(1)
    print(asked)
    sys.exit(0)

if args[0] == "dist-tag":
    name, _, tag_version = args[2].rpartition("@")
    state["latest"][name] = tag_version
    save()
    sys.exit(0)

if args[0] == "publish":
    if not args[1].startswith(("/", "./", "../")) and "/" in args[1]:
        # npm reads `owner/name` as a GitHub repository and clones it.
        print("npm error code 128\\nnpm error git dep preparation failed", file=sys.stderr)
        sys.exit(128)
    name = name_of(args[1])
    if name in state["tlog"]:
        print(
            "npm error code TLOG_CREATE_ENTRY_ERROR\\nnpm error error creating tlog entry - (409) an "
            "equivalent entry already exists in the transparency log",
            file=sys.stderr,
        )
        sys.exit(1)
    if name in state["flaky"]:
        state["flaky"].remove(name)
        save()
        print("npm error code E503\\nnpm error 503 Service Unavailable", file=sys.stderr)
        sys.exit(1)
    state["published"].append(f"{{name}}@{{version}}")
    if name not in state["drift"]:
        state["latest"][name] = version
    save()
    print(f"+ {{name}}@{{version}}")
    sys.exit(0)

sys.exit(2)
'''


def run_script(
    tmp_path: Path, sdk: str | None = None, settle: float = 0, **state: object
) -> tuple[subprocess.CompletedProcess[str], dict]:
    """Run the publish script against a fresh fixture registry."""
    state = {
        "version": VERSION,
        "published": [],
        "latest": {},
        "tlog": [],
        "flaky": [],
        "drift": [],
        "lag": {},
        **state,
    }
    (tmp_path / "registry.json").write_text(json.dumps(state))
    bin_directory = tmp_path / "bin"
    bin_directory.mkdir(exist_ok=True)
    npm = bin_directory / "npm"
    npm.write_text(FAKE_NPM.format(python=sys.executable))
    npm.chmod(0o755)
    platforms = tmp_path / "platforms"
    for target in TARGETS:
        (platforms / target).mkdir(parents=True)
    (tmp_path / "packages" / "sdk").mkdir(parents=True)
    environment = {
        **os.environ,
        "PATH": f"{bin_directory}{os.pathsep}{os.environ['PATH']}",
        "FAKE_NPM_STATE": str(tmp_path / "registry.json"),
    }
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--version",
            VERSION,
            "--platforms",
            str(platforms),
            "--sdk",
            sdk or str(tmp_path / "packages" / "sdk"),
            "--settle",
            str(settle),
        ],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        env=environment,
        check=False,
    )
    return result, json.loads((tmp_path / "registry.json").read_text())


def test_a_full_run_moves_every_latest_tag(tmp_path: Path) -> None:
    result, state = run_script(tmp_path)
    assert result.returncode == 0, result.stderr
    assert state["latest"] == {"fastbrowse": VERSION, **{f"@fastbrowse/{t}": VERSION for t in TARGETS}}
    assert f"npm serves {VERSION} as `latest` for 6 packages" in result.stdout


def test_a_transparency_log_conflict_does_not_stop_the_other_packages(tmp_path: Path) -> None:
    """The 0.9.1 run: two published, the third collided, and the loop ended there."""
    result, state = run_script(
        tmp_path,
        published=[f"@fastbrowse/darwin-arm64@{VERSION}", f"@fastbrowse/darwin-x64@{VERSION}"],
        latest={"@fastbrowse/darwin-arm64": VERSION, "@fastbrowse/darwin-x64": VERSION},
        tlog=["@fastbrowse/linux-arm64"],
    )
    assert result.returncode == 1
    # Every later platform package was still attempted.
    assert f"@fastbrowse/linux-x64@{VERSION}" in state["published"]
    assert f"@fastbrowse/win32-x64@{VERSION}" in state["published"]
    # The SDK is withheld, because it would point at a version that is not there.
    assert f"fastbrowse@{VERSION}" not in state["published"]
    assert "Not publishing fastbrowse@0.9.1" in result.stderr
    assert "@fastbrowse/linux-arm64" in result.stderr
    # The collision is named as one a retry cannot clear, with the command that can.
    assert "--provenance=false" in result.stdout


def test_an_already_published_package_is_passed_over(tmp_path: Path) -> None:
    result, state = run_script(
        tmp_path,
        published=[f"@fastbrowse/linux-arm64@{VERSION}"],
        latest={"@fastbrowse/linux-arm64": VERSION},
    )
    assert result.returncode == 0, result.stderr
    assert f"@fastbrowse/linux-arm64@{VERSION} is already published" in result.stdout
    assert state["published"].count(f"@fastbrowse/linux-arm64@{VERSION}") == 1


def test_a_transient_failure_is_retried(tmp_path: Path) -> None:
    result, _ = run_script(tmp_path, flaky=["@fastbrowse/linux-x64"])
    assert result.returncode == 0, result.stderr
    assert f"@fastbrowse/linux-x64@{VERSION} on the second attempt" in result.stdout


def test_a_published_version_whose_latest_tag_did_not_move_fails(tmp_path: Path) -> None:
    """The 0.9.1 state itself: the version is there, and `npm install` still gets the old one."""
    result, state = run_script(tmp_path, drift=["fastbrowse"])
    assert result.returncode == 1
    assert f"npm dist-tag add fastbrowse@{VERSION} latest" in result.stderr
    assert state["latest"].get("fastbrowse") != VERSION


def test_a_latest_tag_the_registry_cache_shows_late_is_not_drift(tmp_path: Path) -> None:
    # 0.10.0: the SDK's publish was accepted, and the first read of `latest` still showed 0.9.1.
    result, _ = run_script(tmp_path, settle=1, lag={"fastbrowse": 1})
    assert result.returncode == 0, result.stderr
    assert f"npm serves {VERSION} as `latest` for 6 packages" in result.stdout


def test_the_sdk_given_as_the_workflow_gives_it_is_published(tmp_path: Path) -> None:
    # release.yml passes `--sdk packages/sdk`, relative to the checkout.
    result, state = run_script(tmp_path, sdk="packages/sdk")
    assert result.returncode == 0, result.stderr
    assert f"fastbrowse@{VERSION}" in state["published"]
