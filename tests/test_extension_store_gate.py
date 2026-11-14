import json
from pathlib import Path

from scripts.extension_store_gate import COMPAT, blocking

ROOT = Path(__file__).parents[1]
SNAPSHOT = ROOT / "crates" / "fastbrowse-browser" / "extension-store" / "published"


def test_a_release_waits_while_the_store_package_cannot_relay_what_it_needs() -> None:
    problem = blocking({"store": {"version": "0.1.0", "needs": "0.1.1", "missing": ["Runtime.callFunctionOn"]}})
    assert problem is not None
    assert "0.1.1" in problem and "Runtime.callFunctionOn" in problem


def test_a_release_goes_ahead_once_the_store_package_relays_everything() -> None:
    assert blocking({"store": {"version": "0.1.1", "needs": "0.1.1", "missing": []}}) is None


def test_the_recorded_gap_is_the_one_the_checked_in_store_package_has() -> None:
    compat = json.loads(COMPAT.read_text(encoding="utf-8"))
    served = SNAPSHOT / compat["store"]["version"]
    relayed = set(json.loads((served / "allowed-methods.json").read_text(encoding="utf-8")))
    assert compat["store"]["missing"] == [m for m in compat["store"]["missing"] if m not in relayed]
    assert set(compat["store"]["missing"]) == set(compat["store"]["missing"]) - relayed
