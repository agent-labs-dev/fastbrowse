import re
from pathlib import Path

from scripts import changelog


def test_release_headings_and_unreleased_compare_have_current_links() -> None:
    text = (Path(__file__).parents[1] / "CHANGELOG.md").read_text()
    versions = re.findall(r"^## \[(\d+\.\d+\.\d+)\]", text, re.MULTILINE)
    links = dict(re.findall(r"^\[([^]]+)\]: (\S+)$", text, re.MULTILINE))
    root = "https://github.com/agent-labs-dev/fastbrowse"
    for version in versions:
        assert links.get(version) == f"{root}/releases/tag/v{version}"
    assert links["unreleased"] == f"{root}/compare/v{versions[0]}...HEAD"


def test_release_notes_cut_an_oversized_entry_at_an_item_and_link_the_rest() -> None:
    body = "\n".join(f"- **Change {n}.** " + "x" * 90 for n in range(40))
    notes = changelog.release_notes(body, "1.2.3", limit=1000)
    assert len(notes) <= 1000
    assert notes.startswith("- **Change 0.**")
    assert notes.splitlines()[-1].endswith("(https://github.com/agent-labs-dev/fastbrowse/blob/v1.2.3/CHANGELOG.md).")
    assert all(line == "" or line.startswith(("- **Change", "The release notes")) for line in notes.splitlines())
    assert changelog.release_notes("- short", "1.2.3", limit=1000) == "- short"


def test_every_entry_fits_a_github_release() -> None:
    text = changelog.CHANGELOG.read_text(encoding="utf-8")
    for version, body in changelog.entries(text).items():
        assert len(changelog.release_notes(body, version)) <= 125_000, version
