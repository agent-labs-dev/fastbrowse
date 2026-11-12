"""Opus-only v5 retains the dev13 request contract across the OpenRouter Jev migration."""

import hashlib
import json

import httpx
import pytest

from fastbrowse_evals import judging, official


@pytest.mark.parametrize(
    ("policy", "questions", "request_sha", "policy_sha"),
    [
        (
            "browser-internal-opus-v5",
            judging.EVIDENCE_QUESTIONS,
            "607c1af280074a5b853e5d1cd7c1d18bb579f9639985e44f9e3633ee686b0ef4",
            "e697ca113f12e9ac2b1dcae724a792721e67b47d1504a46e8990b4f5bfff7ca1",
        ),
        (
            official.INTERRUPTION_POLICY,
            official.INTERRUPTION_QUESTIONS,
            "a5ec68cb4be0f91e3a2a17e13fed7332e67a8a78d8d3ca8d6698394b4f8d02c6",
            "147afeab1a0b7ce370062bf5059598699487f7a39829f1e98fea1c4ea5ba702d",
        ),
    ],
)
async def test_opus_keeps_its_pre_migration_request_and_receipt_identity(
    monkeypatch, policy, questions, request_sha, policy_sha
):
    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", judging.OPUS_JUDGE)
    monkeypatch.setenv("PARALLAX_JUDGE_BASE_URL", judging.OPENROUTER_BASE_URL)
    monkeypatch.setenv("PARALLAX_JUDGE_API_KEY", "offline-fixture")
    monkeypatch.delattr(judging.jev, "UNBILLED_REJECTIONS", raising=False)
    state = {
        "task": "Read the form",
        "pages": [{"id": "page-1", "url": "https://form.test", "text": "Name: Ada"}],
        "image": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=",
    }
    requests = []

    def transport(request):
        requests.append(request)
        assert str(request.url) == "https://openrouter.ai/api/v1/chat/completions"
        # Captured offline from Parallax dev13: pins instructions, schema, text and image parts together.
        assert hashlib.sha256(request.content).hexdigest() == request_sha
        return httpx.Response(
            200,
            json={
                "id": "offline",
                "choices": [
                    {
                        "message": {
                            "content": json.dumps(
                                {"items": [{"q": key, "yes": True, "why": "Fixture"} for key in questions]}
                            )
                        },
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "cost": 0},
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(transport)) as client:
        result, identity = await judging.judge_browser(
            state,
            policy=policy,
            questions=questions,
            use_jev=False,
            client=client,
        )
    assert identity == policy_sha
    assert len(requests) == 1
    assert result.jev_call is None
    assert all(decision.passed for decision in result.decisions.values())
