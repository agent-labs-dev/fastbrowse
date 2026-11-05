import base64
import json
from datetime import UTC, datetime
from decimal import Decimal

import httpx
import pytest
from pydantic import ValidationError

from fastbrowse_evals.request_quotes import RequestPriceProfile, quote_request
from fastbrowse_evals.spend import SpendStop


def profile(**changes: object) -> RequestPriceProfile:
    return RequestPriceProfile.model_validate(
        {
            "profile_id": "fixture-context-rate-v1",
            "credential_id": "fixture-owned-key",
            "endpoint": "https://provider.test/v1/systemone",
            "model": "fixture/decision",
            "shape": "systemone",
            "verified_at": datetime(2026, 1, 1, tzinfo=UTC),
            "sources": ["https://provider.test/docs/models"],
            "source_sha256": "a" * 64,
            "provider_input_tokens": 1000,
            "provider_output_tokens": 100,
            "input_usd_per_token": "0.000001",
            "output_usd_per_token": "0",
            **changes,
        }
    )


def payload(**changes: object) -> dict[str, object]:
    return {
        "model": "fixture/decision",
        "state": "Untrusted page text.",
        "questions": {"q": {"type": "noul", "instructions": "Is the form complete?"}},
        **changes,
    }


def request(**changes: object) -> httpx.Request:
    return httpx.Request("POST", "https://provider.test/v1/systemone", json=payload(**changes))


def test_quote_uses_documented_context_and_exact_rates_not_body_length() -> None:
    quote = quote_request(profile(conservative_input_passes=3, request_usd="0.000000000000000001"), request())
    assert quote.reserve_usd == Decimal("0.003000000000000001")
    longer = quote_request(profile(), request(state="x" * 5000))
    assert longer.reserve_usd == Decimal("0.001")


def test_choice_and_noul_are_priced_as_text_without_mutating_request() -> None:
    outgoing = request(questions={"q": {"type": "choice", "instructions": "Choose.", "criteria": {"a": "A", "b": "B"}}})
    before = outgoing.content
    assert quote_request(profile(), outgoing).reserve_usd == Decimal("0.001")
    assert outgoing.content == before


def test_systemone_batch_reserves_each_question_with_explicit_conservative_passes() -> None:
    question = {"type": "noul", "instructions": "Is the form complete?"}
    quote = quote_request(profile(conservative_input_passes=3), request(questions={"a": question, "b": question}))
    assert quote.input_passes == 6
    assert quote.reserve_usd == Decimal("0.006")


@pytest.mark.parametrize(
    "changes",
    [
        {"model": "other/model"},
        {"images": ["https://image.test/picture"]},
        {"assets": [{"type": "image", "url": "https://image.test/picture"}]},
        {"questions": {}},
        {"questions": {"q": {"type": "unknown", "instructions": "Q"}}},
        {"questions": {"q": {"type": "noul", "instructions": "Q", "extra": True}}},
        {"provider": {"allow_fallbacks": True}},
        {"plugins": [{"id": "paid-search"}]},
        {"stream": True},
    ],
)
def test_unpriced_shapes_are_denied(changes: dict[str, object]) -> None:
    with pytest.raises(SpendStop):
        quote_request(profile(), request(**changes))


@pytest.mark.parametrize(
    "url",
    ["http://provider.test/v1/systemone", "https://provider.test/other", "https://provider.test/v1/systemone?extra=1"],
)
def test_routes_are_bound(url: str) -> None:
    with pytest.raises(SpendStop):
        quote_request(profile(), httpx.Request("POST", url, json=payload()))


def test_duplicate_json_fields_and_unpriced_encoding_are_denied() -> None:
    duplicate = httpx.Request(
        "POST", profile().endpoint, headers={"content-type": "application/json"}, content=b'{"model":"a","model":"b"}'
    )
    with pytest.raises(SpendStop):
        quote_request(profile(), duplicate)
    encoded = request()
    encoded.headers["content-encoding"] = "gzip"
    with pytest.raises(SpendStop):
        quote_request(profile(), encoded)


def test_quote_fingerprint_binds_body_profile_and_forwarded_headers() -> None:
    original = quote_request(profile(), request())
    assert quote_request(profile(), request(state="different")).quote_id != original.quote_id
    assert quote_request(profile(source_sha256="b" * 64), request()).quote_id != original.quote_id
    assert quote_request(profile(long_context=True), request()).quote_id != original.quote_id
    inherited = request()
    inherited.headers["authorization"] = "Bearer inherited"
    assert quote_request(profile(), inherited).quote_id == original.quote_id
    inherited.headers["typesafe-danger-zone-long-context"] = "true"
    with pytest.raises(SpendStop):
        quote_request(profile(), inherited)


def test_chat_reserves_provider_output_without_adding_a_hidden_cap() -> None:
    pricing = profile(
        shape="chat_completions", endpoint="https://provider.test/v1/chat/completions", output_usd_per_token="0.000002"
    )
    body = {"model": "fixture/decision", "messages": [{"role": "user", "content": "Hello"}]}
    outgoing = httpx.Request("POST", pricing.endpoint, json=body)
    assert quote_request(pricing, outgoing).reserve_usd == Decimal("0.0012")
    assert "max_tokens" not in outgoing.content.decode()
    assert quote_request(
        pricing, httpx.Request("POST", pricing.endpoint, json=body | {"max_tokens": 20})
    ).reserve_usd == Decimal("0.00104")
    for extra in ({"max_tokens": 101}, {"stream": True}, {"tools": []}):
        with pytest.raises(SpendStop):
            quote_request(pricing, httpx.Request("POST", pricing.endpoint, json=body | extra))


def test_json_schema_numeric_constraints_are_priced_without_changing_body() -> None:
    pricing = profile(shape="chat_completions", endpoint="https://provider.test/v1/chat/completions")
    outgoing = httpx.Request(
        "POST",
        pricing.endpoint,
        json={
            "model": pricing.model,
            "messages": [{"role": "user", "content": "Read the amount."}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "amount", "schema": {"type": "number", "minimum": 0.1}},
            },
        },
    )
    assert quote_request(pricing, outgoing).reserve_usd == Decimal("0.001")


def test_documented_structured_state_and_question_text_are_priced() -> None:
    outgoing = request(
        state={"page": {"title": "Order", "amount": 4.6}, "controls": [{"role": "button", "label": "Continue"}]},
        questions={
            "q": {
                "type": "choice",
                "instructions": {"question": "Choose the next action."},
                "criteria": {"a": ["Read", {"id": 1}]},
            }
        },
    )
    before = outgoing.content
    assert quote_request(profile(), outgoing).reserve_usd == Decimal("0.001")
    assert outgoing.content == before


def test_provider_reasoning_and_usage_must_match_source_bound_profile_exactly() -> None:
    options = {
        "provider": {"require_parameters": True, "sort": "latency", "max_price": {"prompt": 1.2}},
        "reasoning": {"effort": "none"},
        "usage": {"include": True},
    }
    pricing = profile(request_options_json=json.dumps(options))
    assert quote_request(pricing, request(**options)).reserve_usd == Decimal("0.001")
    assert quote_request(pricing, request(**options)).quote_id != quote_request(profile(), request()).quote_id
    for change in ({}, options | {"provider": {"sort": "price"}}, options | {"reasoning": {"effort": "high"}}):
        with pytest.raises(SpendStop):
            quote_request(pricing, request(**change))


@pytest.mark.parametrize(
    "options", ['{"plugins":[]}', '{"provider":{"api_key":"unpriced"}}', '{"usage":{"extra":true}}']
)
def test_profile_cannot_authorize_arbitrary_service_options(options: str) -> None:
    with pytest.raises(ValidationError):
        profile(request_options_json=options)


@pytest.mark.parametrize("field", ["usage", "provider", "reasoning"])
def test_numeric_values_cannot_impersonate_pinned_boolean_options(field: str) -> None:
    names = {"usage": "include", "provider": "allow_fallbacks", "reasoning": "enabled"}
    pricing = profile(request_options_json=json.dumps({field: {names[field]: True}}))
    with pytest.raises(SpendStop):
        quote_request(pricing, request(**{field: {names[field]: 1}}))


@pytest.mark.parametrize(
    "options",
    [
        {"stream": 0},
        {"response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": {}, "strict": 1}}},
    ],
)
def test_chat_protocol_booleans_do_not_accept_numeric_values(options: dict[str, object]) -> None:
    pricing = profile(shape="chat_completions", endpoint="https://provider.test/v1/chat/completions")
    outgoing = httpx.Request(
        "POST",
        pricing.endpoint,
        json={"model": pricing.model, "messages": [{"role": "user", "content": "Read."}], **options},
    )
    with pytest.raises(SpendStop):
        quote_request(pricing, outgoing)


@pytest.mark.parametrize(
    "changes",
    [
        {"input_usd_per_token": 0.1},
        {"endpoint": "http://provider.test/v1/systemone"},
        {"endpoint": "https://key@provider.test/v1/systemone"},
    ],
)
def test_invalid_profiles_are_rejected(changes: dict[str, object]) -> None:
    with pytest.raises(ValidationError):
        profile(**changes)


def test_source_bound_image_profile_prices_assets_and_box_without_rewriting() -> None:
    outgoing = request(
        state={"screenshot": "fixture-image"},
        assets={
            "fixture-image": {
                "type": "image",
                "format": "png",
                "data_base64": base64.b64encode(b"\x89PNG\r\n\x1a\nfixture").decode(),
            }
        },
        questions={"target": {"type": "box", "instructions": {"find": "Continue in fixture-image"}}},
    )
    before = outgoing.content
    quoted = quote_request(profile(images=True), outgoing)
    assert quoted.reserve_usd == Decimal("0.001")
    assert outgoing.content == before
    with pytest.raises(SpendStop):
        quote_request(profile(), outgoing)


def test_schema_diagnostic_names_field_and_type_without_input_values() -> None:
    outgoing = request(assets={"sensitive-asset-reference": {"url": "https://private.test/secret"}})
    with pytest.raises(SpendStop) as stopped:
        quote_request(profile(), outgoing)
    assert "assets" in str(stopped.value)
    assert "sensitive-asset-reference" not in str(stopped.value)
    assert "private.test" not in str(stopped.value)


@pytest.mark.parametrize(
    "fmt, signature", [("png", b"\x89PNG\r\n\x1a\n"), ("jpg", b"\xff\xd8\xff"), ("jpeg", b"\xff\xd8\xff")]
)
def test_inline_image_format_and_source_bound_flag(fmt: str, signature: bytes) -> None:
    outgoing = request(assets={"image": {"format": fmt, "data_base64": base64.b64encode(signature).decode()}})
    assert quote_request(profile(images=True), outgoing).reserve_usd == Decimal("0.001")
    assert (
        quote_request(profile(images=True), request()).profile_sha256
        != quote_request(profile(), request()).profile_sha256
    )
    for value in [1, "true"]:
        with pytest.raises(ValidationError):
            profile(images=value)
    with pytest.raises(ValidationError):
        profile(images=True, shape="chat_completions")


@pytest.mark.parametrize(
    "asset",
    [
        {"format": "png", "data_base64": "https://private.test/image"},
        {"format": "png", "data_base64": "data:image/png;base64,aA=="},
        {"format": "png", "data_base64": "aA==\n"},
        {"format": "png", "data_base64": "aA"},
        {"format": "png", "data_base64": "aB=="},
        {"format": "png", "data_base64": "aA=="},
        {"format": "png", "data_base64": base64.b64encode(b"\xff\xd8\xff").decode()},
        {"format": "gif", "data_base64": base64.b64encode(b"GIF89a").decode()},
        {"format": "png", "data_base64": "", "url": "https://private.test/image"},
        {"format": "png", "part": "multipart-file"},
    ],
)
def test_image_profile_rejects_remote_malformed_and_unpriced_assets(asset: dict[str, str]) -> None:
    with pytest.raises(SpendStop) as stopped:
        quote_request(profile(images=True), request(assets={"sensitive-reference": asset}))
    assert "sensitive-reference" not in str(stopped.value)
    assert "private.test" not in str(stopped.value)


def test_images_header_is_owned_and_box_needs_images_without_criteria() -> None:
    outgoing = request()
    outgoing.headers["typesafe-danger-zone-images"] = "true"
    with pytest.raises(SpendStop, match="images header"):
        quote_request(profile(), outgoing)
    outgoing.headers["typesafe-danger-zone-images"] = "false"
    with pytest.raises(SpendStop, match="images header"):
        quote_request(profile(images=True), outgoing)
    with pytest.raises(SpendStop, match="require an image"):
        quote_request(profile(images=True), request(questions={"q": {"type": "box", "instructions": "Find it."}}))
    with pytest.raises(SpendStop):
        quote_request(
            profile(images=True), request(questions={"q": {"type": "box", "instructions": "Find it.", "criteria": {}}})
        )


def test_typesafe_documented_image_file_and_request_body_limits() -> None:
    image = b"\x89PNG\r\n\x1a\n" + b"x" * (16 * 1024 * 1024 - 7)
    with pytest.raises(SpendStop, match="assets:value_error"):
        quote_request(
            profile(images=True),
            request(assets={"image": {"format": "png", "data_base64": base64.b64encode(image).decode()}}),
        )
    with pytest.raises(SpendStop, match="TypeSafe's documented body limit"):
        quote_request(profile(images=True), request(state="x" * (32 * 1024 * 1024)))


def test_chat_inline_images_require_source_bound_opt_in_and_reserve_every_image() -> None:
    pricing = profile(
        shape="chat_completions",
        endpoint="https://provider.test/v1/chat/completions",
        chat_images=True,
        image_usd_per_image="0.002",
    )
    image = {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
    outgoing = httpx.Request(
        "POST",
        pricing.endpoint,
        json={
            "model": pricing.model,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Read."}, image, image]}],
        },
    )
    before = outgoing.content
    assert quote_request(pricing, outgoing).reserve_usd == Decimal("0.005")
    assert outgoing.content == before
    assert pricing.owned_headers() == {"content-type": "application/json"}
    with pytest.raises(SpendStop):
        quote_request(pricing.model_copy(update={"chat_images": False}), outgoing)


@pytest.mark.parametrize("value", [1, "true", 0.1])
def test_chat_image_profile_requires_strict_flag(value: object) -> None:
    with pytest.raises(ValidationError):
        profile(shape="chat_completions", chat_images=value, image_usd_per_image="0")


def test_chat_image_profile_requires_explicit_price_and_matching_shape() -> None:
    for changes in (
        {"shape": "chat_completions", "chat_images": True},
        {"chat_images": True, "image_usd_per_image": "0"},
        {"image_usd_per_image": "0"},
        {"shape": "chat_completions", "chat_images": True, "image_usd_per_image": 0.1},
    ):
        with pytest.raises(ValidationError):
            profile(**changes)


@pytest.mark.parametrize(
    "part",
    [
        {"type": "image_url", "image_url": {"url": "https://private.test/secret"}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo=\n"}},
        {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,iVBORw0KGgo="}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgp="}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo=", "detail": "high"}},
        {"type": "input_audio", "input_audio": {"data": "secret"}},
        {"type": "text", "text": "Read.", "unknown": "secret"},
    ],
)
def test_unpriced_chat_parts_are_rejected_without_reflecting_values(part: dict[str, object]) -> None:
    pricing = profile(shape="chat_completions", chat_images=True, image_usd_per_image="0")
    outgoing = httpx.Request(
        "POST",
        pricing.endpoint,
        json={
            "model": pricing.model,
            "messages": [{"role": "user", "content": [part]}],
        },
    )
    with pytest.raises(SpendStop) as error:
        quote_request(pricing, outgoing)
    assert "messages:" in str(error.value)
    assert "secret" not in str(error.value)
    assert "private.test" not in str(error.value)


@pytest.mark.parametrize("image_url", ["data:image/png;base64,iVBORw0KGgo=", "data:image/jpeg;base64,/9j/"])
def test_chat_core_image_formats_and_profile_hash(image_url: str) -> None:
    pricing = profile(shape="chat_completions", chat_images=True, image_usd_per_image="0")
    outgoing = httpx.Request(
        "POST",
        pricing.endpoint,
        json={
            "model": pricing.model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Read."},
                        {"type": "image_url", "image_url": {"url": image_url}},
                    ],
                }
            ],
        },
    )
    original = quote_request(pricing, outgoing)
    higher = quote_request(pricing.model_copy(update={"image_usd_per_image": Decimal("0.003")}), outgoing)
    assert higher.reserve_usd == original.reserve_usd + Decimal("0.003")
    assert higher.profile_sha256 != original.profile_sha256
