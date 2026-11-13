"""A published policy keeps its identity when Parallax verifies another fallback model."""

import dataclasses

from parallax import images
from parallax import judging as pj

from fastbrowse_evals import judging, live


def test_a_new_verified_model_leaves_the_published_identity_unchanged(monkeypatch):
    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", judging.JUDGE_MODEL)
    monkeypatch.setenv("PARALLAX_JUDGE_BASE_URL", judging.OPENROUTER_BASE_URL)
    monkeypatch.setenv("PARALLAX_JUDGE_API_KEY", "offline-fixture")

    def identity() -> str:
        panel, llm, primary = judging._route(False)
        return judging._identity(live.JUDGE_POLICY, judging.JUDGE_QUESTIONS, panel, llm, primary, use_jev=False)

    before = identity()
    other = "example/verified-later"
    monkeypatch.setitem(pj.FALLBACK_CONTEXT_TOKENS, other, 1)
    monkeypatch.setitem(images.VISION, other, dataclasses.replace(images.VISION[judging.JUDGE_MODEL], max_images=1))
    assert identity() == before
