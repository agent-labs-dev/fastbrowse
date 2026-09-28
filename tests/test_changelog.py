import re
from pathlib import Path


def test_release_headings_and_unreleased_compare_have_current_links() -> None:
    text = (Path(__file__).parents[1] / "CHANGELOG.md").read_text()
    versions = re.findall(r"^## \[(\d+\.\d+\.\d+)\]", text, re.MULTILINE)
    links = dict(re.findall(r"^\[([^]]+)\]: (\S+)$", text, re.MULTILINE))
    root = "https://github.com/agent-labs-dev/fastbrowse"
    for version in versions:
        assert links.get(version) == f"{root}/releases/tag/v{version}"
    assert links["unreleased"] == f"{root}/compare/v{versions[0]}...HEAD"
