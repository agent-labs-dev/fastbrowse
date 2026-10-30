//! The LLM providers FASTBROWSE_LLM_PROVIDER names, each a preset over the OpenAI-compatible client: where it lives,
//! which variable holds its key, how it takes a schema and a reasoning effort, and the models it defaults to. Anthropic
//! alone speaks its native Messages API, since its OpenAI-compatible endpoint ignores the schema.
//!
//! Every value here is checked against the provider's own documentation; docs/llm-providers.md cites each one.

use fastbrowse_core::models::LLMPurpose;

use crate::openai_compatible::{OutputFormat, ReasoningField, Wire};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LLMProvider {
    OpenRouter,
    Gateway,
    OpenAI,
    Anthropic,
    Gemini,
    Groq,
    Together,
    Mistral,
    DeepSeek,
    XAI,
    Fireworks,
    Azure,
    Ollama,
    LMStudio,
    VLLM,
    /// Any other OpenAI-compatible endpoint at FASTBROWSE_LLM_BASE_URL.
    Custom,
}

/// Where a provider's key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// The provider's own variable, which is sent to its host and nowhere else.
    Variable(&'static str),
    /// FASTBROWSE_LLM_API_KEY, optional: a server on the caller's own machine or network.
    Local,
}

/// Where a provider's requests go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// A fixed host. FASTBROWSE_LLM_BASE_URL is refused, since it would carry the provider's key elsewhere.
    Hosted(&'static str),
    /// A host the caller names in FASTBROWSE_LLM_BASE_URL, which must end with one of these suffixes.
    Resource(&'static [&'static str]),
    /// A server the caller runs: this address unless FASTBROWSE_LLM_BASE_URL names another.
    Local(&'static str),
    /// FASTBROWSE_LLM_BASE_URL, required.
    Custom,
}

/// How a provider's OpenAI-compatible endpoint is spoken to.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub provider: LLMProvider,
    pub wire: Wire,
    pub endpoint: Endpoint,
    pub key: KeySource,
    /// The header carrying the key in place of `Authorization: Bearer`.
    pub auth_header: Option<&'static str>,
    pub output_format: OutputFormat,
    /// Whether the endpoint documents strict JSON schema decoding. FASTBROWSE_LLM_STRICT overrides it.
    pub strict: bool,
    /// Where the effort goes, or None for an endpoint that ignores or refuses one.
    pub reasoning: Option<ReasoningField>,
    /// Per-purpose models when the provider has ids worth defaulting to; otherwise FASTBROWSE_LLM_MODEL is required.
    pub models: Option<fn(LLMPurpose) -> &'static str>,
}

const BASE: Preset = Preset {
    provider: LLMProvider::Custom,
    wire: Wire::ChatCompletions,
    endpoint: Endpoint::Custom,
    key: KeySource::Local,
    auth_header: None,
    output_format: OutputFormat::JsonSchema,
    strict: true,
    reasoning: Some(ReasoningField::OpenRouter),
    models: None,
};

/// Sonnet plans, reads and verifies; Haiku writes field text and shortcuts, whose output is checked downstream, as
/// flash-lite does on OpenRouter.
fn claude_model(purpose: LLMPurpose) -> &'static str {
    match purpose {
        LLMPurpose::FieldText | LLMPurpose::Shortcut => "claude-haiku-5-5",
        _ => "claude-sonnet-5-5",
    }
}

/// Groq documents strict decoding on a few models only; gpt-oss-120b is the largest of them.
fn groq_model(_: LLMPurpose) -> &'static str {
    "openai/gpt-oss-120b"
}

impl LLMProvider {
    pub const ALL: [LLMProvider; 16] = [
        LLMProvider::OpenRouter,
        LLMProvider::Gateway,
        LLMProvider::OpenAI,
        LLMProvider::Anthropic,
        LLMProvider::Gemini,
        LLMProvider::Groq,
        LLMProvider::Together,
        LLMProvider::Mistral,
        LLMProvider::DeepSeek,
        LLMProvider::XAI,
        LLMProvider::Fireworks,
        LLMProvider::Azure,
        LLMProvider::Ollama,
        LLMProvider::LMStudio,
        LLMProvider::VLLM,
        LLMProvider::Custom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LLMProvider::OpenRouter => "openrouter",
            LLMProvider::Gateway => "gateway",
            LLMProvider::OpenAI => "openai",
            LLMProvider::Anthropic => "anthropic",
            LLMProvider::Gemini => "gemini",
            LLMProvider::Groq => "groq",
            LLMProvider::Together => "together",
            LLMProvider::Mistral => "mistral",
            LLMProvider::DeepSeek => "deepseek",
            LLMProvider::XAI => "xai",
            LLMProvider::Fireworks => "fireworks",
            LLMProvider::Azure => "azure",
            LLMProvider::Ollama => "ollama",
            LLMProvider::LMStudio => "lmstudio",
            LLMProvider::VLLM => "vllm",
            LLMProvider::Custom => "custom",
        }
    }

    pub fn parse(value: &str) -> Option<LLMProvider> {
        LLMProvider::ALL.into_iter().find(|provider| provider.as_str() == value)
    }

    /// The name a log line gives the provider. A custom endpoint keeps the name it had before presets existed, so a
    /// recorded route reads the same.
    pub fn label(self) -> &'static str {
        match self {
            LLMProvider::OpenRouter => "OpenRouter",
            LLMProvider::Gateway => "Vercel AI Gateway",
            LLMProvider::OpenAI => "OpenAI",
            LLMProvider::Anthropic => "Anthropic",
            LLMProvider::Gemini => "Gemini",
            LLMProvider::Groq => "Groq",
            LLMProvider::Together => "Together AI",
            LLMProvider::Mistral => "Mistral",
            LLMProvider::DeepSeek => "DeepSeek",
            LLMProvider::XAI => "xAI",
            LLMProvider::Fireworks => "Fireworks",
            LLMProvider::Azure => "Azure OpenAI",
            LLMProvider::Ollama => "Ollama",
            LLMProvider::LMStudio => "LM Studio",
            LLMProvider::VLLM => "vLLM",
            LLMProvider::Custom => "local endpoint",
        }
    }

    /// The preset for every provider but OpenRouter and the gateway, which route by model and keep their own path.
    pub fn preset(self) -> Preset {
        let hosted = |url, variable| Preset {
            provider: self,
            endpoint: Endpoint::Hosted(url),
            key: KeySource::Variable(variable),
            reasoning: Some(ReasoningField::Effort),
            ..BASE
        };
        match self {
            LLMProvider::OpenRouter | LLMProvider::Gateway | LLMProvider::Custom => Preset { provider: self, ..BASE },
            LLMProvider::OpenAI => hosted("https://api.openai.com/v1", "OPENAI_API_KEY"),
            // Anthropic's compatibility layer ignores response_format and reasoning_effort, so Claude is reached
            // through the Messages API, whose structured outputs decode against the schema and which takes an effort.
            LLMProvider::Anthropic => Preset {
                wire: Wire::AnthropicMessages,
                models: Some(claude_model),
                ..hosted("https://api.anthropic.com/v1", "ANTHROPIC_API_KEY")
            },
            LLMProvider::Gemini => hosted("https://generativelanguage.googleapis.com/v1beta/openai", "GEMINI_API_KEY"),
            LLMProvider::Groq => {
                Preset { models: Some(groq_model), ..hosted("https://api.groq.com/openai/v1", "GROQ_API_KEY") }
            }
            // Together and Fireworks decode against the schema but document no strict flag or effort.
            LLMProvider::Together => {
                Preset { strict: false, reasoning: None, ..hosted("https://api.together.ai/v1", "TOGETHER_API_KEY") }
            }
            LLMProvider::Fireworks => Preset {
                strict: false,
                reasoning: None,
                ..hosted("https://api.fireworks.ai/inference/v1", "FIREWORKS_API_KEY")
            },
            LLMProvider::Mistral => Preset { strict: false, ..hosted("https://api.mistral.ai/v1", "MISTRAL_API_KEY") },
            // DeepSeek's JSON output takes json_object only, and its reasoning is a model choice rather than a field.
            LLMProvider::DeepSeek => Preset {
                output_format: OutputFormat::JsonObject,
                strict: false,
                reasoning: None,
                ..hosted("https://api.deepseek.com", "DEEPSEEK_API_KEY")
            },
            LLMProvider::XAI => hosted("https://api.x.ai/v1", "XAI_API_KEY"),
            // The v1 API takes the OpenAI request shape at the caller's own resource, keyed by an api-key header.
            LLMProvider::Azure => Preset {
                endpoint: Endpoint::Resource(&[".openai.azure.com", ".services.ai.azure.com"]),
                auth_header: Some("api-key"),
                ..hosted("", "AZURE_OPENAI_API_KEY")
            },
            LLMProvider::Ollama => Preset {
                endpoint: Endpoint::Local("http://localhost:11434/v1"),
                reasoning: Some(ReasoningField::Effort),
                ..Preset { provider: self, ..BASE }
            },
            LLMProvider::LMStudio => {
                Preset { endpoint: Endpoint::Local("http://localhost:1234/v1"), ..Preset { provider: self, ..BASE } }
            }
            LLMProvider::VLLM => {
                Preset { endpoint: Endpoint::Local("http://localhost:8000/v1"), ..Preset { provider: self, ..BASE } }
            }
        }
    }
}

impl std::fmt::Display for LLMProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
