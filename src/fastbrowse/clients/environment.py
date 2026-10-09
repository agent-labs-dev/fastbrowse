"""Settings from the environment and `.env`, and the default Jev and LLM clients built from them.

Jev: TYPESAFE_API_KEY selects direct TypeSafe; otherwise OPENROUTER_API_KEY is primary, with
AI_GATEWAY_API_KEY (Vercel AI Gateway) as backup when set, or primary when neither other key is set.
FASTBROWSE_JEV_SOURCE picks openrouter, typesafe or gateway explicitly. OpenRouter and direct use the
gateway as backup; the gateway uses direct when configured, otherwise OpenRouter.
FASTBROWSE_JEV_BASE_URL points either at a proxy or another host serving the same API, and
FASTBROWSE_JEV_MODEL pins an OpenRouter or direct model. A custom endpoint or model disables automatic failover.
LLM: OPENROUTER_API_KEY; when it is absent, AI_GATEWAY_API_KEY serves the same models through the Vercel AI
Gateway's OpenAI-compatible chat completions.
Cloud browser: BROWSER_USE_API_KEY. FASTBROWSE_LLM_MODEL overrides every purpose at once, and
FASTBROWSE_LLM_MODEL_<PURPOSE> (PLAN, READ, FIELD_TEXT, RECOVER, COMPOSE, VERIFY, SHORTCUT) overrides one.
FASTBROWSE_LLM_REASONING sets the reasoning effort: low (default), medium or high. FASTBROWSE_CHROME
names the Chrome binary; FASTBROWSE_HEADED=1 shows its window and FASTBROWSE_PROFILE keeps its profile
between runs, and either one selects local Chrome. `.env.example` lists them all. A real environment variable
beats `.env`.
"""

from enum import StrEnum
from pathlib import Path
from typing import assert_never

import httpx
from pydantic import AliasChoices, Field, SecretStr, ValidationError
from pydantic_settings import BaseSettings, SettingsConfigDict

from fastbrowse.clients.failover import FailoverJevClient
from fastbrowse.clients.openai_compatible import (
    GATEWAY_PROVIDER_ROUTING,
    OPENROUTER_PROVIDER_ROUTING,
    OpenAICompatibleLLM,
    ReasoningEffort,
)
from fastbrowse.clients.typesafe import OPENROUTER_MODEL, OPENROUTER_URL, TYPESAFE_URL, TypeSafeJevClient
from fastbrowse.clients.vercel import GATEWAY_URL, VercelGatewayJevClient
from fastbrowse.jev import JEV_MODEL, JevClient
from fastbrowse.llm import LLMClient
from fastbrowse.models import LLMPurpose, LocalChrome

# The default is the model the live suite passes on. gemini-3.5-flash-lite is two to three times faster
# a call but scored 8/12 live against 12/12, so it serves only the purposes whose output is checked
# downstream. FASTBROWSE_LLM_MODEL overrides every purpose (docs/evals.md has the measurements).
DEFAULT_LLM = "google/gemini-3.8-flash"
FIELD_TEXT_LLM = "google/gemini-3.5-flash-lite"

# Each of these fails visibly rather than into the answer: field text is typed and seen to work or not, a
# shortcut is an address confined to the start origin. Planning defines output checks, so an invented subject
# can reject correctly quoted answers even when the done check agrees the task is finished. VERIFY is the last
# word on what the done check doubted, so it
# stays on the default: on flash-lite it passed a Google Flights search with no nonstop filter ten times in ten,
# reading the "Nonstop" rows as the filter, where gemini-3.8-flash refused all ten and passed the filtered page.
DEFAULT_MODELS = dict.fromkeys(LLMPurpose, DEFAULT_LLM) | {
    LLMPurpose.FIELD_TEXT: FIELD_TEXT_LLM,
    LLMPurpose.SHORTCUT: FIELD_TEXT_LLM,
}

# gemini-3.8-flash rejects disabled reasoning but accepts less of it: `low` took plan 4.6s to 3.2s and
# read 2.4s to 2.1s. FASTBROWSE_LLM_REASONING overrides it.
DEFAULT_REASONING = ReasoningEffort.LOW


class JevSource(StrEnum):
    OPENROUTER = "openrouter"
    TYPESAFE = "typesafe"
    GATEWAY = "gateway"


class LLMSource(StrEnum):
    OPENROUTER = "openrouter"
    GATEWAY = "gateway"


_LLM_LABELS = {LLMSource.OPENROUTER: "OpenRouter", LLMSource.GATEWAY: "Vercel AI Gateway"}
LLM_OPENROUTER_URL = "https://openrouter.ai/api/v1"
LLM_GATEWAY_URL = "https://ai-gateway.vercel.sh/v1"


class ConfigurationError(RuntimeError):
    """A missing key or an invalid setting in the environment, reported without a traceback."""


def _key(name: str) -> SecretStr | None:
    return Field(default=None, validation_alias=AliasChoices(name))


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_file=".env", env_prefix="FASTBROWSE_", extra="ignore", frozen=True)

    typesafe_api_key: SecretStr | None = _key("TYPESAFE_API_KEY")
    ai_gateway_api_key: SecretStr | None = _key("AI_GATEWAY_API_KEY")
    openrouter_api_key: SecretStr | None = _key("OPENROUTER_API_KEY")
    browser_use_api_key: SecretStr | None = _key("BROWSER_USE_API_KEY")
    mcp_token: SecretStr | None = None
    """Bearer token for `fastbrowse-mcp --transport http`. A declared field rather than a bare
    `os.environ` read, because `.env.example` tells you to put it in `.env` and only a field reads that."""
    jev_source: JevSource | None = None
    jev_base_url: str | None = None
    jev_model: str | None = None
    llm_model: str | None = None
    llm_model_plan: str | None = None
    llm_model_read: str | None = None
    llm_model_field_text: str | None = None
    llm_model_recover: str | None = None
    llm_model_compose: str | None = None
    llm_model_verify: str | None = None
    llm_model_shortcut: str | None = None
    llm_reasoning: ReasoningEffort = DEFAULT_REASONING
    chrome: str | None = None
    headed: bool = False
    profile: Path | None = None

    def models(self) -> dict[LLMPurpose, str]:
        """Per-purpose models, most specific setting winning."""
        return {
            purpose: self._purpose_model(purpose) or self.llm_model or DEFAULT_MODELS[purpose] for purpose in LLMPurpose
        }

    def _purpose_model(self, purpose: LLMPurpose) -> str | None:
        match purpose:
            case LLMPurpose.PLAN:
                return self.llm_model_plan
            case LLMPurpose.READ:
                return self.llm_model_read
            case LLMPurpose.FIELD_TEXT:
                return self.llm_model_field_text
            case LLMPurpose.RECOVER:
                return self.llm_model_recover
            case LLMPurpose.COMPOSE:
                return self.llm_model_compose
            case LLMPurpose.VERIFY:
                return self.llm_model_verify
            case LLMPurpose.SHORTCUT:
                return self.llm_model_shortcut
            case _:
                assert_never(purpose)

    def local_chrome(self) -> LocalChrome:
        return LocalChrome(binary=self.chrome, headed=self.headed, profile=self.profile)

    def jev_route(self) -> tuple[JevSource, JevSource | None]:
        """The Jev provider a run starts on, and the one it fails over to when that one's retries run out."""
        source = self.jev_source
        if source is None:
            if self.typesafe_api_key:
                source = JevSource.TYPESAFE
            else:
                source = JevSource.OPENROUTER if self.openrouter_api_key else JevSource.GATEWAY
        # A proxy may be a routing boundary, and the gateway cannot honour a direct-API model pin.
        if self.jev_base_url or self.jev_model is not None:
            return source, None
        match source:
            case JevSource.OPENROUTER | JevSource.TYPESAFE:
                return source, JevSource.GATEWAY if self.ai_gateway_api_key else None
            case JevSource.GATEWAY:
                if self.typesafe_api_key:
                    return source, JevSource.TYPESAFE
                return source, JevSource.OPENROUTER if self.openrouter_api_key else None
            case _:
                assert_never(source)

    def providers(self) -> str:
        """Which providers a run will call, for the first line of a log: an outage reads differently with no backup."""
        source, backup = self.jev_route()
        failover = f"backup {backup}" if backup is not None else "no backup: an outage past its retries ends the run"
        models = ", ".join(sorted(set(self.models().values())))
        return f"Jev {source} ({failover}); LLM {models} via {_LLM_LABELS[self.llm_source()]}"

    def llm_source(self) -> LLMSource:
        """Where the LLM goes: OpenRouter when keyed, otherwise the gateway, which serves the same models."""
        return LLMSource.OPENROUTER if self.openrouter_api_key else LLMSource.GATEWAY

    def jev(self, http: httpx.AsyncClient) -> JevClient:
        source, backup_source = self.jev_route()
        primary = self._jev_client(source, http)
        if backup_source is None:
            return primary
        return FailoverJevClient(primary, self._jev_client(backup_source, http))

    def _jev_client(self, source: JevSource, http: httpx.AsyncClient) -> JevClient:
        match source:
            case JevSource.OPENROUTER:
                if not self.openrouter_api_key:
                    raise ConfigurationError("set OPENROUTER_API_KEY for Jev via OpenRouter")
                return TypeSafeJevClient(
                    self.openrouter_api_key.get_secret_value(),
                    http=http,
                    base_url=self.jev_base_url or OPENROUTER_URL,
                    model=self.jev_model or OPENROUTER_MODEL,
                )
            case JevSource.TYPESAFE:
                if not self.typesafe_api_key:
                    raise ConfigurationError("set TYPESAFE_API_KEY for Jev via the direct API")
                return TypeSafeJevClient(
                    self.typesafe_api_key.get_secret_value(),
                    http=http,
                    base_url=self.jev_base_url or TYPESAFE_URL,
                    model=self.jev_model or JEV_MODEL,
                )
            case JevSource.GATEWAY:
                if not self.ai_gateway_api_key:
                    raise ConfigurationError(
                        "set AI_GATEWAY_API_KEY for Jev via the gateway, OPENROUTER_API_KEY or TYPESAFE_API_KEY"
                    )
                return VercelGatewayJevClient(
                    self.ai_gateway_api_key.get_secret_value(), http=http, base_url=self.jev_base_url or GATEWAY_URL
                )
            case _:
                assert_never(source)

    def llm(self, http: httpx.AsyncClient) -> LLMClient:
        return self._llm_client(self.llm_source(), http)

    def _llm_client(self, source: LLMSource, http: httpx.AsyncClient) -> LLMClient:
        match source:
            case LLMSource.OPENROUTER:
                if not self.openrouter_api_key:
                    raise ConfigurationError("set OPENROUTER_API_KEY or AI_GATEWAY_API_KEY for the LLM")
                key, base_url, routing = self.openrouter_api_key, LLM_OPENROUTER_URL, OPENROUTER_PROVIDER_ROUTING
            case LLMSource.GATEWAY:
                if not self.ai_gateway_api_key:
                    raise ConfigurationError("set AI_GATEWAY_API_KEY or OPENROUTER_API_KEY for the LLM")
                key, base_url, routing = self.ai_gateway_api_key, LLM_GATEWAY_URL, GATEWAY_PROVIDER_ROUTING
            case _:
                assert_never(source)
        return OpenAICompatibleLLM(
            key.get_secret_value(),
            http=http,
            base_url=base_url,
            models=self.models(),
            reasoning_effort=self.llm_reasoning,
            provider_routing=routing,
        )

    def openrouter_key(self) -> str:
        if not self.openrouter_api_key:
            raise ConfigurationError("set OPENROUTER_API_KEY for the LLM")
        return self.openrouter_api_key.get_secret_value()

    def browser_key(self) -> str:
        if not self.browser_use_api_key:
            raise ConfigurationError("set BROWSER_USE_API_KEY for a cloud browser")
        return self.browser_use_api_key.get_secret_value()


def load_settings() -> Settings:
    try:
        return Settings()
    except ValidationError as exc:
        # Name the setting, never echo the input: the field may be a key.
        fields = ", ".join(".".join(str(part) for part in error["loc"]) for error in exc.errors())
        raise ConfigurationError(f"invalid settings: {fields}") from None
