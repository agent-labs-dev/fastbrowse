import json

import httpx
import pytest

from fastbrowse_evals import arm_pins, live

LATEST = {
    "https://pypi.org/pypi/browser-use/json": {"info": {"version": "9.9.9"}},
    "https://pypi.org/pypi/browser-harness/json": {"info": {"version": "0.1.13"}},
    "https://pypi.org/pypi/browser-use-sdk/json": {"info": {"version": "3.12.1"}},
    "https://registry.npmjs.org/@openai/codex/latest": {"version": "0.167.1"},
}


@pytest.fixture
def pins(tmp_path, monkeypatch):
    path = tmp_path / "arm-pins.json"
    path.write_text(
        json.dumps(
            {
                "browser-use": {"registry": "pypi", "version": "1.0.0"},
                "browser-harness": {"registry": "pypi", "version": "0.1.13"},
                "@openai/codex": {"registry": "npm", "version": "0.167.1"},
            }
        )
    )
    monkeypatch.setattr(arm_pins, "PINS_PATH", path)
    monkeypatch.setattr(arm_pins, "installed_version", lambda _name: "3.12.1")
    transport = httpx.MockTransport(lambda request: httpx.Response(200, json=LATEST[str(request.url)]))
    real = httpx.Client
    monkeypatch.setattr(arm_pins.httpx, "Client", lambda **_kwargs: real(transport=transport))
    return path


def test_check_names_only_the_pin_behind_its_latest_release(pins, capsys):
    assert arm_pins.main(["check"]) == 1
    assert capsys.readouterr().out == "browser-use (pypi): pinned 1.0.0, latest 9.9.9\n"


def test_update_moves_the_pin_and_check_then_passes(pins):
    assert arm_pins.main(["update"]) == 0
    assert json.loads(pins.read_text())["browser-use"]["version"] == "9.9.9"
    assert arm_pins.main(["check"]) == 0


def test_every_arm_pin_comes_from_the_pin_file():
    pinned = arm_pins.load()
    assert f"browser-use=={pinned['browser-use']['version']}" == live.OSS_PACKAGE
    assert pinned["@openai/codex"]["version"] in live.HARNESS_PIN and pinned["cua-driver"]["version"] in live.CUA_PIN


def test_check_with_arms_ignores_pins_those_arms_do_not_install(pins):
    assert arm_pins.main(["check", "--arms", "fastbrowse", "browser-harness"]) == 0
    assert arm_pins.main(["check", "--arms", "browser-use-oss"]) == 1


def test_every_comparison_arm_names_its_pins():
    assert set(arm_pins.ARM_PINS) == {arm for arm in live.ARMS if not arm.startswith("fastbrowse")}
    assert {name for names in arm_pins.ARM_PINS.values() for name in names} == {*arm_pins.load(), arm_pins.SDK}
