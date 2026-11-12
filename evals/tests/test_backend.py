import pytest

from fastbrowse_evals import backend


def test_configured_search_key_reaches_the_product_but_not_an_isolated_adapter(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        backend,
        "environment",
        lambda: {"OPENROUTER_API_KEY": "text", "EXA_API_KEY": "search", "BROWSER_USE_API_KEY": "browser"},
    )
    configured = backend.run_settings()
    assert configured["EXA_API_KEY"] == "search"
    assert "BROWSER_USE_API_KEY" not in configured
    isolated = backend.run_settings({"FASTBROWSE_LLM_PROVIDER": "custom"})
    assert "EXA_API_KEY" not in isolated
    assert "OPENROUTER_API_KEY" not in isolated
