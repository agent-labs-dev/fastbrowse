"""Conservative, source-bound reservations for restricted request shapes.

Profiles are supplied by the operator after checking provider documentation. They
are estimates, not provider guarantees, and do not authorize a campaign themselves.
"""

from __future__ import annotations

import base64
import hashlib
import json
from decimal import Decimal, localcontext
from typing import Annotated, Literal

import httpx
from parallax.models import Model
from pydantic import (
    AwareDatetime,
    Field,
    HttpUrl,
    JsonValue,
    StrictBool,
    TypeAdapter,
    ValidationError,
    field_validator,
    model_validator,
)

from fastbrowse_evals.spend import Money, SpendStop


class RequestPriceProfile(Model):
    profile_id: str = Field(min_length=1)
    credential_id: str = Field(min_length=1)
    endpoint: str
    model: str = Field(min_length=1)
    shape: Literal["systemone", "chat_completions"]
    verified_at: AwareDatetime
    sources: tuple[HttpUrl, ...] = Field(min_length=1)
    source_sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    provider_input_tokens: int = Field(gt=0, strict=True)
    provider_output_tokens: int = Field(ge=0, strict=True)
    input_usd_per_token: Money
    output_usd_per_token: Money
    request_usd: Money = Decimal(0)
    conservative_input_passes: int = Field(default=1, gt=0, strict=True)
    long_context: StrictBool = False
    images: StrictBool = False
    chat_images: StrictBool = False
    image_usd_per_image: Money | None = None
    request_options_json: str = "{}"

    @field_validator("input_usd_per_token", "output_usd_per_token", "request_usd", "image_usd_per_image", mode="before")
    @classmethod
    def exact_rates(cls, value: object) -> object:
        if isinstance(value, (float, bool)):
            raise ValueError("prices must be exact decimal strings or Decimal values")
        return value

    @model_validator(mode="after")
    def valid_endpoint(self) -> RequestPriceProfile:
        url = httpx.URL(self.endpoint)
        if url.scheme != "https" or not url.host or url.userinfo or url.query or url.fragment:
            raise ValueError("a price profile requires one HTTPS endpoint without credentials, query or fragment")
        if self.shape == "chat_completions" and self.provider_output_tokens == 0:
            raise ValueError("chat profiles need the provider's documented output limit")
        if self.shape != "systemone" and (self.long_context or self.images):
            raise ValueError("the long-context and images headers belong to System One")
        if self.chat_images and (self.shape != "chat_completions" or self.image_usd_per_image is None):
            raise ValueError("chat images require a chat profile and an explicit documented per-image rate")
        if not self.chat_images and self.image_usd_per_image is not None:
            raise ValueError("per-image pricing requires chat images")
        if ":" in self.model:
            raise ValueError("model routing and service suffixes are outside the priced text shapes")
        json_object(self.request_options_json.encode())
        _Options.model_validate_json(self.request_options_json)
        return self

    def owned_headers(self) -> dict[str, str]:
        headers = {"content-type": "application/json"}
        if self.long_context:
            headers["typesafe-danger-zone-long-context"] = "true"
        if self.images:
            headers["typesafe-danger-zone-images"] = "true"
        return headers


TextData = str | dict[str, JsonValue] | list[JsonValue]


class _Provider(Model):
    require_parameters: StrictBool | None = None
    sort: Literal["price", "throughput", "latency"] | None = None
    only: tuple[str, ...] | None = None
    order: tuple[str, ...] | None = None
    allow_fallbacks: StrictBool | None = None
    max_price: dict[Literal["prompt", "completion", "request", "image"], Decimal] | None = None


class _Reasoning(Model):
    effort: str | None = None
    max_tokens: int | None = Field(default=None, gt=0, strict=True)
    exclude: StrictBool | None = None
    enabled: StrictBool | None = None


class _Usage(Model):
    include: StrictBool


class _Options(Model):
    provider: _Provider | None = None
    reasoning: _Reasoning | None = None
    reasoning_effort: str | None = None
    usage: _Usage | None = None


class _Noul(Model):
    type: Literal["noul"]
    instructions: TextData
    criteria: dict[Literal["true", "false"], TextData] | None = None


class _Choice(Model):
    type: Literal["choice"]
    instructions: TextData
    criteria: dict[str, TextData] = Field(min_length=1)


# TypeSafe documents these upload limits at https://docs.typesafe.ai/images#limits.
TYPESAFE_IMAGE_FILE_BYTES = 16 * 1024 * 1024
TYPESAFE_IMAGE_REQUEST_BYTES = 32 * 1024 * 1024


class _Image(Model):
    type: Literal["image"] = "image"
    format: Literal["png", "jpg", "jpeg"]
    data_base64: str = Field(min_length=1)

    @model_validator(mode="after")
    def inline_file(self) -> _Image:
        decoded = _decoded_image(self.format, self.data_base64)
        if len(decoded) > TYPESAFE_IMAGE_FILE_BYTES:
            raise ValueError("image exceeds TypeSafe's documented file limit")
        # The provider owns full decoding, dimensions and expanded-reference limits.
        return self


class _Box(Model):
    type: Literal["box"]
    instructions: TextData


class _SystemOne(_Options):
    model: str
    state: TextData
    questions: dict[str, Annotated[_Noul | _Choice | _Box, Field(discriminator="type")]] = Field(min_length=1)
    assets: dict[str, _Image] | None = None


def _decoded_image(image_format: str, encoded: str) -> bytes:
    decoded = base64.b64decode(encoded, validate=True)
    if base64.b64encode(decoded).decode("ascii") != encoded:
        raise ValueError("images require canonical standard base64")
    signature = b"\x89PNG\r\n\x1a\n" if image_format == "png" else b"\xff\xd8\xff"
    if not decoded.startswith(signature):
        raise ValueError("image signature does not match its declared format")
    return decoded


class _InlineImageUrl(Model):
    url: str

    @field_validator("url")
    @classmethod
    def inline_png_or_jpeg(cls, value: str) -> str:
        prefix, separator, encoded = value.partition(",")
        if not separator or prefix not in ("data:image/png;base64", "data:image/jpeg;base64"):
            raise ValueError("chat images require inline PNG or JPEG data URLs")
        _decoded_image("png" if prefix == "data:image/png;base64" else "jpeg", encoded)
        return value


class _TextPart(Model):
    type: Literal["text"]
    text: str


class _ImagePart(Model):
    type: Literal["image_url"]
    image_url: _InlineImageUrl


class _Message(Model):
    role: Literal["system", "developer", "user", "assistant"]
    content: str | tuple[Annotated[_TextPart | _ImagePart, Field(discriminator="type")], ...]


class _JsonSchema(Model):
    name: str
    strict: StrictBool | None = None
    schema_: dict[str, JsonValue] = Field(alias="schema")


class _ResponseFormat(Model):
    type: Literal["json_schema"]
    json_schema: _JsonSchema


class _Chat(_Options):
    model: str
    messages: tuple[_Message, ...] = Field(min_length=1)
    max_tokens: int | None = Field(default=None, gt=0, strict=True)
    temperature: Decimal | None = None
    response_format: _ResponseFormat | None = None
    stream: Literal[False] = False

    @field_validator("stream", mode="before")
    @classmethod
    def nonstreaming(cls, value: object) -> Literal[False]:
        if value is not False:
            raise ValueError("only an actual JSON false is priced for streaming")
        return False


class RequestQuote(Model):
    quote_id: str
    profile_id: str
    profile_sha256: str
    request_sha256: str
    input_passes: int = Field(gt=0)
    reserve_usd: Money = Field(gt=0)


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate request or receipt field")
        result[key] = value
    return result


def json_object(body: bytes) -> dict[str, object]:
    """Keep monetary decimals exact and reject ambiguous JSON before admission."""

    def invalid_constant(_: str) -> object:
        raise ValueError("nonfinite JSON number")

    value = json.loads(body, parse_float=Decimal, parse_constant=invalid_constant, object_pairs_hook=_unique_object)
    if not isinstance(value, dict):
        raise ValueError("the request or receipt must be a JSON object")
    return value


def quote_request(profile: RequestPriceProfile, request: httpx.Request) -> RequestQuote:
    if request.method != "POST" or request.url != httpx.URL(profile.endpoint):
        raise SpendStop("no price profile for this request route")
    content_type = request.headers.get("content-type", "").lower()
    if content_type not in ("application/json", "application/json; charset=utf-8"):
        raise SpendStop("only uncompressed JSON requests are priced")
    if "content-encoding" in request.headers:
        raise SpendStop("encoded request bodies are not priced")
    expected_long = "true" if profile.long_context else "false"
    if request.headers.get("typesafe-danger-zone-long-context", expected_long).lower() != expected_long:
        raise SpendStop("the long-context header differs from the price profile")
    expected_images = "true" if profile.images else "false"
    if request.headers.get("typesafe-danger-zone-images", expected_images).lower() != expected_images:
        raise SpendStop("the images header differs from the price profile")
    if profile.images and len(request.content) > TYPESAFE_IMAGE_REQUEST_BYTES:
        raise SpendStop("image request exceeds TypeSafe's documented body limit")
    schema = _SystemOne if profile.shape == "systemone" else _Chat
    try:
        raw = json_object(request.content)
        expected = json_object(profile.request_options_json.encode())
        provided = {key: raw[key] for key in _Options.model_fields if key in raw}
        if provided != expected:
            raise ValueError("provider, reasoning or usage options differ from the price profile")
        payload = schema.model_validate_json(request.content)
        if isinstance(payload, _SystemOne):
            boxed = any(isinstance(question, _Box) for question in payload.questions.values())
            if (payload.assets is not None or boxed) and not profile.images:
                raise SpendStop("assets and Box questions require an approved image profile")
            if boxed and not payload.assets:
                raise SpendStop("Box questions require an image asset")
        image_count = 0
        if isinstance(payload, _Chat):
            image_count = sum(
                isinstance(part, _ImagePart)
                for message in payload.messages
                if not isinstance(message.content, str)
                for part in message.content
            )
            if image_count and not profile.chat_images:
                raise SpendStop("chat images require an approved chat image profile")
        if payload.model != profile.model:
            raise ValueError("the model differs from the price profile")
        output = profile.provider_output_tokens
        passes = profile.conservative_input_passes * (len(payload.questions) if isinstance(payload, _SystemOne) else 1)
        if isinstance(payload, _Chat) and payload.max_tokens is not None:
            if payload.max_tokens > output:
                raise ValueError("the request exceeds the provider's documented output limit")
            output = payload.max_tokens
        # Reserve the documented context, not a byte-count approximation to its tokenizer.
        with localcontext() as context:
            context.prec = 100 + len(str(profile.provider_input_tokens)) + len(str(output))
            reserve = (
                profile.input_usd_per_token * profile.provider_input_tokens * passes
                + profile.output_usd_per_token * output
                + profile.request_usd
                + (profile.image_usd_per_image or Decimal(0)) * image_count
            )
        reserve = TypeAdapter(Money).validate_python(reserve)
        forwarded_headers = profile.owned_headers()
        fingerprint = hashlib.sha256(
            json.dumps(
                {"method": "POST", "url": str(request.url), "headers": forwarded_headers}, sort_keys=True
            ).encode()
            + b"\n"
            + request.content
        ).hexdigest()
        profile_hash = hashlib.sha256(profile.model_dump_json().encode()).hexdigest()
        return RequestQuote(
            quote_id=hashlib.sha256(f"{profile_hash}:{fingerprint}".encode()).hexdigest(),
            profile_id=profile.profile_id,
            profile_sha256=profile_hash,
            request_sha256=fingerprint,
            input_passes=passes,
            reserve_usd=reserve,
        )
    except ValidationError as exc:
        errors = exc.errors(include_url=False, include_context=False, include_input=False)
        fields = sorted(
            {
                f"{error['loc'][0] if error['loc'] and error['loc'][0] in schema.model_fields else '*'}:{error['type']}"
                for error in errors
            }
        )
        raise SpendStop("request schema rejected: " + ", ".join(fields)) from None
    except (ValueError, TypeError, ArithmeticError) as exc:
        raise SpendStop(f"request is outside the priced shape: {type(exc).__name__}") from None
