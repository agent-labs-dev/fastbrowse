"""The doc check has to catch the drift it exists for, not just pass on a clean tree."""

from pathlib import Path

from scripts.docs_sync import (
    check_changelog,
    check_env,
    check_flags,
    check_links,
    check_media,
    check_statuses,
)


def test_a_documented_flag_the_parser_lacks_is_reported() -> None:
    copy = {"README.md": "| `--ghost NAME` | nothing does this |\n"}
    assert check_flags(copy, 'Opt::value("--real")') == ["README.md: `--ghost` is documented but no parser defines it"]


def test_a_documented_flag_the_parser_has_passes() -> None:
    copy = {"README.md": "| `--real NAME` | the same thing |\n"}
    assert check_flags(copy, 'Opt::value("--real")') == []


def test_a_flag_named_only_in_prose_is_not_a_declared_flag() -> None:
    copy = {"README.md": "| `--gone` | removed |\n"}
    code = 'parser.add_argument("--kept")\nraise ValueError("--gone was removed")'
    assert check_flags(copy, code) == ["README.md: `--gone` is documented but no parser defines it"]


def test_an_argparse_declaration_defines_a_flag() -> None:
    copy = {"README.md": "| `--kept` | kept |\n"}
    assert check_flags(copy, 'parser.add_argument(\n        "--kept", action="store_true")') == []


def test_a_prose_mention_of_a_flag_is_not_a_table_claim() -> None:
    copy = {"README.md": "Pass the debugging flag to Chrome.\n"}
    assert check_flags(copy, "") == []


def test_a_setting_nothing_reads_is_reported() -> None:
    copy = {"README.md": "Set FASTBROWSE_GHOST to turn this on.\n"}
    assert check_env(copy, "FASTBROWSE_REAL") == ["README.md: FASTBROWSE_GHOST is documented but nothing reads it"]


def _status_repo(tmp_path: Path) -> Path:
    models = tmp_path / "src" / "fastbrowse" / "models.py"
    models.parent.mkdir(parents=True)
    models.write_text(
        "class Status(StrEnum):\n"
        '    COMPLETE = "complete"\n'
        '    STUCK = "stuck"\n'
        "\n"
        "\n"
        "class Other(StrEnum):\n"
        '    X = "x"\n',
        encoding="utf-8",
    )
    cli = tmp_path / "src" / "fastbrowse" / "cli.py"
    cli.write_text("EXIT_CODES = {\n    Status.COMPLETE: 0,\n    Status.STUCK: 9,\n}\n", encoding="utf-8")
    return tmp_path


def test_the_status_table_and_the_exit_codes_agree(tmp_path: Path) -> None:
    root = _status_repo(tmp_path)
    copy = {
        "README.md": "| Status | Exit |\n| `complete` | 0 |\n| `stuck` | 9 |\n",
        "docs/troubleshooting.md": "## stuck (exit 9)\n",
    }
    assert check_statuses(root, copy) == []


def test_a_status_missing_from_the_readme_table_is_reported(tmp_path: Path) -> None:
    root = _status_repo(tmp_path)
    copy = {"README.md": "| `complete` | 0 |\n", "docs/troubleshooting.md": "## stuck (exit 9)\n"}
    assert check_statuses(root, copy) == ["README.md: the status table has no row for `stuck`"]


def test_an_exit_code_that_drifted_from_the_cli_is_reported(tmp_path: Path) -> None:
    root = _status_repo(tmp_path)
    copy = {
        "README.md": "| `complete` | 0 |\n| `stuck` | 8 |\n",
        "docs/troubleshooting.md": "## stuck (exit 9)\n",
    }
    assert check_statuses(root, copy) == ["README.md: `stuck` exits 8, but the CLI exits 9"]


def test_a_status_with_no_exit_code_is_reported(tmp_path: Path) -> None:
    root = _status_repo(tmp_path)
    cli = root / "src" / "fastbrowse" / "cli.py"
    cli.write_text("EXIT_CODES = {\n    Status.COMPLETE: 0,\n}\n", encoding="utf-8")
    copy = {
        "README.md": "| `complete` | 0 |\n| `stuck` | 9 |\n",
        "docs/troubleshooting.md": "## stuck (exit 9)\n",
    }
    assert check_statuses(root, copy) == ["src/fastbrowse/cli.py: `stuck` has no exit code in EXIT_CODES"]


def test_a_troubleshooting_section_that_drifted_is_reported(tmp_path: Path) -> None:
    root = _status_repo(tmp_path)
    copy = {
        "README.md": "| `complete` | 0 |\n| `stuck` | 9 |\n",
        "docs/troubleshooting.md": "## stuck (exit 7)\n",
    }
    assert check_statuses(root, copy) == ["docs/troubleshooting.md: `stuck` says exit 7, but the CLI exits 9"]


def test_a_relative_link_to_nothing_is_reported(tmp_path: Path) -> None:
    (tmp_path / "docs").mkdir()
    copy = {"README.md": "Read [the guide](docs/missing.md).\n", "docs/kept.md": "Nothing here.\n"}
    assert check_links(tmp_path, copy) == ["README.md: `docs/missing.md` does not exist"]


def test_a_relative_link_that_resolves_from_its_own_directory_passes(tmp_path: Path) -> None:
    (tmp_path / "docs").mkdir()
    (tmp_path / "docs" / "kept.md").write_text("Kept.", encoding="utf-8")
    copy = {"docs/kept.md": "Read [the README](../README.md).\n"}
    (tmp_path / "README.md").write_text("Read me.", encoding="utf-8")
    assert check_links(tmp_path, copy) == []


def test_a_version_with_no_changelog_entry_is_reported(tmp_path: Path) -> None:
    (tmp_path / "pyproject.toml").write_text('[project]\nversion = "1.2.3"\n', encoding="utf-8")
    copy = {"CHANGELOG.md": "## [1.2.2]\n\n- Older.\n"}
    assert check_changelog(tmp_path, copy) == [
        "CHANGELOG.md: no `## [1.2.3]` entry for the version pyproject.toml declares"
    ]


def test_a_shown_asset_missing_from_the_repository_is_reported(tmp_path: Path) -> None:
    copy = {"README.md": "![a demo](docs/assets/demo.gif)\n"}
    problems = check_media(tmp_path, copy)
    assert any("docs/assets/demo.gif" in problem and "not in the repository" in problem for problem in problems)


def test_media_without_a_record_is_reported(tmp_path: Path) -> None:
    (tmp_path / "docs/assets").mkdir(parents=True)
    (tmp_path / "docs/assets/demo.gif").write_bytes(b"GIF89a")
    copy = {"README.md": "![a demo](docs/assets/demo.gif)\n"}
    problems = check_media(tmp_path, copy)
    assert any("no record" in problem for problem in problems)


def test_a_recording_of_a_surface_we_no_longer_ship_is_reported(tmp_path: Path) -> None:
    (tmp_path / "docs/assets").mkdir(parents=True)
    (tmp_path / "docs/assets/demo.gif").write_bytes(b"GIF89a")
    (tmp_path / "docs/assets/media.json").write_text('{"assets": {"docs/assets/demo.gif": {"surface": "old"}}}\n')
    (tmp_path / "src/fastbrowse").mkdir(parents=True)
    (tmp_path / "src/fastbrowse/models.py").write_text('class Status(StrEnum):\n    COMPLETE = "complete"\n')
    (tmp_path / "src/fastbrowse/cli.py").write_text("EXIT = {Status.COMPLETE: 0}\n")
    copy = {"README.md": "![a demo](docs/assets/demo.gif)\n"}
    problems = check_media(tmp_path, copy)
    assert any("Re-record it" in problem for problem in problems)
