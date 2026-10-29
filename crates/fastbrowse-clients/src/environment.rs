//! Port of `clients/environment.py`: settings from the environment and `.env`, and the default Jev and LLM clients
//! built from them.
//!
//! Jev: TYPESAFE_API_KEY selects direct TypeSafe; otherwise OPENROUTER_API_KEY is primary, with AI_GATEWAY_API_KEY
//! (Vercel AI Gateway) as backup when set, or primary when neither other key is set. FASTBROWSE_DECISION_SOURCE picks
//! openrouter, typesafe or gateway explicitly. OpenRouter and direct use the gateway as backup; the gateway uses direct
//! when configured, otherwise OpenRouter. FASTBROWSE_DECISION_BASE_URL points either at a proxy or another host serving the
//! same API, and FASTBROWSE_DECISION_MODEL pins an OpenRouter or direct model. A custom endpoint or model disables
//! automatic failover. Each FASTBROWSE_DECISION_ variable is still read under its former FASTBROWSE_JEV_ name.
//!
//! LLM: OPENROUTER_API_KEY; when it is absent, AI_GATEWAY_API_KEY serves the same models through the Vercel AI
//! Gateway's OpenAI-compatible chat completions. FASTBROWSE_LLM_BASE_URL points the LLM at any other OpenAI-compatible
//! chat completions endpoint (for example a local vLLM server) instead, and FASTBROWSE_LLM_API_KEY supplies its bearer
//! token when it has one; that endpoint never receives the OpenRouter or gateway key. FASTBROWSE_LLM_STRICT and
//! FASTBROWSE_LLM_EXTRA_BODY shape requests to that endpoint only. FASTBROWSE_LLM_MODEL overrides every purpose at
//! once, and FASTBROWSE_LLM_MODEL_<PURPOSE> overrides one. FASTBROWSE_LLM_REASONING sets the reasoning effort: none,
//! low (default), medium or high, and FASTBROWSE_LLM_REASONING_<PURPOSE> sets one purpose's.
//!
//! FASTBROWSE_LLM_PROVIDER names the LLM provider outright instead: openrouter, gateway, openai, anthropic, gemini,
//! groq, together, mistral, deepseek, xai, fireworks, azure, ollama, lmstudio, vllm or custom. Each reads its own key
//! variable and sends it to its own host only; [`crate::providers`] holds what each one takes.
//!
//! Cloud browser: BROWSER_USE_API_KEY. FASTBROWSE_CHROME names the Chrome binary; FASTBROWSE_HEADED=1 shows its window
//! and FASTBROWSE_PROFILE keeps its profile between runs. A real environment variable beats `.env`.
//!
//! pydantic-settings reads names without regard to case, prefixes every field but the four keys with `FASTBROWSE_`,
//! and validates in lax mode; [`Settings::from_sources`] does the same.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use fastbrowse_core::decision::{DecisionClient, JEV_MODEL};
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{LLMPurpose, LocalChrome};
use fastbrowse_core::py::lax::bool_of;
use serde_json::{Map, Value};

use crate::anthropic::AnthropicLLM;
use crate::failover::FailoverDecisionClient;
use crate::http::Http;
use crate::openai_compatible::{
    LLMOptions, OpenAICompatibleLLM, ReasoningEffort, ReasoningField, Wire, gateway_provider_routing,
    openrouter_provider_routing, reserved_clashes,
};
use crate::openai_decisions::{OPENAI_DECISIONS_MODEL, OPENAI_URL, OpenAIDecisionsClient};
pub use crate::providers::LLMProvider;
use crate::providers::{Endpoint, KeySource, Preset};
use crate::system_one::{
    AIMLAPI_MODEL, AIMLAPI_URL, DECISIONS_PATH, OLLAMA_URL, OPENROUTER_MODEL, OPENROUTER_URL, PERPLEXITY_MODEL,
    PERPLEXITY_URL, SystemOneClient, TELNYX_MODEL, TELNYX_URL, TYPESAFE_URL,
};
use crate::vercel::{GATEWAY_URL, VercelGatewayJevClient};

/// The default is the model the live suite passes on. gemini-3.5-flash-lite is two to three times faster a call but
/// scored 8/12 live against 12/12, so it serves only the purposes whose output is checked downstream.
/// FASTBROWSE_LLM_MODEL overrides every purpose (docs/evals.md has the measurements).
pub const DEFAULT_LLM: &str = "google/gemini-3.8-flash";
pub const FIELD_TEXT_LLM: &str = "google/gemini-3.5-flash-lite";

/// Each flash-lite purpose fails visibly rather than into the answer: field text is typed and seen to work or not, a
/// shortcut is an address confined to the start origin. VERIFY is the last word on what the done check doubted, so it
/// stays on the default: on flash-lite it passed a Google Flights search with no nonstop filter ten times in ten.
pub fn default_model(purpose: LLMPurpose) -> &'static str {
    match purpose {
        LLMPurpose::FieldText | LLMPurpose::Shortcut => FIELD_TEXT_LLM,
        _ => DEFAULT_LLM,
    }
}

/// gemini-3.8-flash rejects disabled reasoning but accepts less of it: `low` took plan 4.6s to 3.2s and read 2.4s to
/// 2.1s. FASTBROWSE_LLM_REASONING overrides it.
pub const DEFAULT_REASONING: ReasoningEffort = ReasoningEffort::Low;

pub const LLM_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";
pub const LLM_GATEWAY_URL: &str = "https://ai-gateway.vercel.sh/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DecisionSource {
    OpenRouter,
    TypeSafe,
    Gateway,
    /// Telnyx's own decision models over the System One API.
    Telnyx,
    /// A local Ollama serving a decision model over the System One API.
    Ollama,
    /// Any other System One host, at FASTBROWSE_DECISION_BASE_URL.
    Custom,
    /// OpenAI's Decisions API.
    OpenAI,
    /// AIMLAPI, reselling Jev, Liquid's d1 and OpenAI's gpt-6-luna.
    Aimlapi,
    /// Perplexity's pplx-decider models.
    Perplexity,
}

impl DecisionSource {
    pub const ALL: [DecisionSource; 9] = [
        DecisionSource::OpenRouter,
        DecisionSource::TypeSafe,
        DecisionSource::Gateway,
        DecisionSource::Telnyx,
        DecisionSource::Ollama,
        DecisionSource::Custom,
        DecisionSource::OpenAI,
        DecisionSource::Aimlapi,
        DecisionSource::Perplexity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DecisionSource::OpenRouter => "openrouter",
            DecisionSource::TypeSafe => "typesafe",
            DecisionSource::Gateway => "gateway",
            DecisionSource::Telnyx => "telnyx",
            DecisionSource::Ollama => "ollama",
            DecisionSource::Custom => "custom",
            DecisionSource::OpenAI => "openai",
            DecisionSource::Aimlapi => "aimlapi",
            DecisionSource::Perplexity => "perplexity",
        }
    }

    /// The sources that serve Jev, so any one can stand in for another in an outage.
    pub const JEV: [DecisionSource; 3] =
        [DecisionSource::OpenRouter, DecisionSource::TypeSafe, DecisionSource::Gateway];

    fn serves_jev(self) -> bool {
        DecisionSource::JEV.contains(&self)
    }

    pub fn parse(value: &str) -> Option<DecisionSource> {
        DecisionSource::ALL.into_iter().find(|source| source.as_str() == value)
    }
}

impl std::fmt::Display for DecisionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A missing key or an invalid setting in the environment, reported without a traceback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationError(pub String);

impl std::fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigurationError {}

/// A key. Never printed: a settings dump in a log must not carry it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `bool(SecretStr(...))`: an empty key, as a blank `KEY=` in `.env` leaves, counts as unset.
    pub fn is_set(&self) -> bool {
        !self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "Secret('')" } else { "Secret('**********')" })
    }
}

fn set(key: &Option<Secret>) -> bool {
    key.as_ref().is_some_and(Secret::is_set)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub typesafe_api_key: Option<Secret>,
    pub ai_gateway_api_key: Option<Secret>,
    pub openrouter_api_key: Option<Secret>,
    pub browser_use_api_key: Option<Secret>,
    /// Bearer token for `fastbrowse-mcp --transport http`.
    pub mcp_token: Option<Secret>,
    pub decision_source: Option<DecisionSource>,
    pub decision_base_url: Option<String>,
    pub decision_model: Option<String>,
    /// Bearer token for the `ollama` and `custom` sources. Optional: a local server usually takes none.
    pub decision_api_key: Option<Secret>,
    pub telnyx_api_key: Option<Secret>,
    pub openai_api_key: Option<Secret>,
    pub aimlapi_api_key: Option<Secret>,
    pub perplexity_api_key: Option<Secret>,
    /// FASTBROWSE_LLM_PROVIDER: the LLM provider named outright rather than inferred from which keys are set.
    pub llm_provider: Option<LLMProvider>,
    /// The key from the variable FASTBROWSE_LLM_PROVIDER's preset reads, such as OPENAI_API_KEY.
    pub llm_provider_api_key: Option<Secret>,
    pub llm_model: Option<String>,
    pub llm_model_plan: Option<String>,
    pub llm_model_read: Option<String>,
    pub llm_model_field_text: Option<String>,
    pub llm_model_recover: Option<String>,
    pub llm_model_compose: Option<String>,
    pub llm_model_verify: Option<String>,
    pub llm_model_shortcut: Option<String>,
    /// A custom OpenAI-compatible chat completions endpoint. When set, the LLM stops going to OpenRouter and the
    /// Vercel AI Gateway and every purpose talks to this host instead.
    pub llm_base_url: Option<String>,
    /// Bearer token for FASTBROWSE_LLM_BASE_URL. Optional: an empty value sends requests without an Authorization
    /// header.
    pub llm_api_key: Option<Secret>,
    /// Send strict structured output. Some local vLLM builds mangle the output under strict mode.
    pub llm_strict: bool,
    /// Whether FASTBROWSE_LLM_STRICT was given: a preset's own default holds until it is.
    pub llm_strict_set: bool,
    /// Extra JSON object merged into every chat completions request body to the custom endpoint. An invalid value
    /// stops the run.
    pub llm_extra_body: Option<String>,
    pub llm_reasoning: ReasoningEffort,
    /// Whether FASTBROWSE_LLM_REASONING was given, as pydantic's `model_fields_set` records it.
    pub llm_reasoning_set: bool,
    /// FASTBROWSE_LLM_REASONING_<PURPOSE>: one purpose's effort over FASTBROWSE_LLM_REASONING, as a purpose's model is.
    pub llm_reasoning_plan: Option<ReasoningEffort>,
    pub llm_reasoning_read: Option<ReasoningEffort>,
    pub llm_reasoning_field_text: Option<ReasoningEffort>,
    pub llm_reasoning_recover: Option<ReasoningEffort>,
    pub llm_reasoning_compose: Option<ReasoningEffort>,
    pub llm_reasoning_verify: Option<ReasoningEffort>,
    pub llm_reasoning_shortcut: Option<ReasoningEffort>,
    pub chrome: Option<String>,
    pub headed: bool,
    pub profile: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            typesafe_api_key: None,
            ai_gateway_api_key: None,
            openrouter_api_key: None,
            browser_use_api_key: None,
            mcp_token: None,
            decision_source: None,
            decision_base_url: None,
            decision_model: None,
            decision_api_key: None,
            telnyx_api_key: None,
            openai_api_key: None,
            aimlapi_api_key: None,
            perplexity_api_key: None,
            llm_provider: None,
            llm_provider_api_key: None,
            llm_model: None,
            llm_model_plan: None,
            llm_model_read: None,
            llm_model_field_text: None,
            llm_model_recover: None,
            llm_model_compose: None,
            llm_model_verify: None,
            llm_model_shortcut: None,
            llm_base_url: None,
            llm_api_key: None,
            llm_strict: true,
            llm_strict_set: false,
            llm_extra_body: None,
            llm_reasoning: DEFAULT_REASONING,
            llm_reasoning_set: false,
            llm_reasoning_plan: None,
            llm_reasoning_read: None,
            llm_reasoning_field_text: None,
            llm_reasoning_recover: None,
            llm_reasoning_compose: None,
            llm_reasoning_verify: None,
            llm_reasoning_shortcut: None,
            chrome: None,
            headed: false,
            profile: None,
        }
    }
}

/// The variables a setting was read from before it took a provider-neutral name, read when the new one is unset.
const FORMERLY: [(&str, &str); 3] = [
    ("fastbrowse_decision_source", "fastbrowse_jev_source"),
    ("fastbrowse_decision_base_url", "fastbrowse_jev_base_url"),
    ("fastbrowse_decision_model", "fastbrowse_jev_model"),
];

/// Each setting's name as pydantic validates it, in declaration order, and the variable it is read from.
const FIELDS: [(&str, &str); 37] = [
    ("TYPESAFE_API_KEY", "typesafe_api_key"),
    ("AI_GATEWAY_API_KEY", "ai_gateway_api_key"),
    ("OPENROUTER_API_KEY", "openrouter_api_key"),
    ("BROWSER_USE_API_KEY", "browser_use_api_key"),
    ("mcp_token", "fastbrowse_mcp_token"),
    ("decision_source", "fastbrowse_decision_source"),
    ("decision_base_url", "fastbrowse_decision_base_url"),
    ("decision_model", "fastbrowse_decision_model"),
    ("decision_api_key", "fastbrowse_decision_api_key"),
    ("TELNYX_API_KEY", "telnyx_api_key"),
    ("OPENAI_API_KEY", "openai_api_key"),
    ("AIMLAPI_API_KEY", "aimlapi_api_key"),
    ("PERPLEXITY_API_KEY", "perplexity_api_key"),
    ("llm_provider", "fastbrowse_llm_provider"),
    ("llm_model", "fastbrowse_llm_model"),
    ("llm_model_plan", "fastbrowse_llm_model_plan"),
    ("llm_model_read", "fastbrowse_llm_model_read"),
    ("llm_model_field_text", "fastbrowse_llm_model_field_text"),
    ("llm_model_recover", "fastbrowse_llm_model_recover"),
    ("llm_model_compose", "fastbrowse_llm_model_compose"),
    ("llm_model_verify", "fastbrowse_llm_model_verify"),
    ("llm_model_shortcut", "fastbrowse_llm_model_shortcut"),
    ("llm_base_url", "fastbrowse_llm_base_url"),
    ("llm_api_key", "fastbrowse_llm_api_key"),
    ("llm_strict", "fastbrowse_llm_strict"),
    ("llm_extra_body", "fastbrowse_llm_extra_body"),
    ("llm_reasoning", "fastbrowse_llm_reasoning"),
    ("llm_reasoning_plan", "fastbrowse_llm_reasoning_plan"),
    ("llm_reasoning_read", "fastbrowse_llm_reasoning_read"),
    ("llm_reasoning_field_text", "fastbrowse_llm_reasoning_field_text"),
    ("llm_reasoning_recover", "fastbrowse_llm_reasoning_recover"),
    ("llm_reasoning_compose", "fastbrowse_llm_reasoning_compose"),
    ("llm_reasoning_verify", "fastbrowse_llm_reasoning_verify"),
    ("llm_reasoning_shortcut", "fastbrowse_llm_reasoning_shortcut"),
    ("chrome", "fastbrowse_chrome"),
    ("headed", "fastbrowse_headed"),
    ("profile", "fastbrowse_profile"),
];

/// `KEY=value` lines as python-dotenv reads them: `export` allowed, quotes stripped (escapes read inside double
/// quotes), and an unquoted value cut at a ` #` comment.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((key, raw)) = line.split_once('=') else { continue };
        let key = key.trim().to_string();
        let raw = raw.trim_start();
        let value = if let Some(rest) = raw.strip_prefix('\'') {
            rest.split_once('\'').map_or(rest, |(value, _)| value).replace("\\'", "'")
        } else if let Some(rest) = raw.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = rest.chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => match chars.next() {
                        Some('n') => value.push('\n'),
                        Some('t') => value.push('\t'),
                        Some('r') => value.push('\r'),
                        Some(other) => value.push(other),
                        None => value.push('\\'),
                    },
                    c => value.push(c),
                }
            }
            value
        } else {
            let cut = raw.char_indices().find(|(i, c)| *c == '#' && raw[..*i].ends_with(char::is_whitespace));
            raw[..cut.map_or(raw.len(), |(i, _)| i)].trim_end().to_string()
        };
        pairs.push((key, value));
    }
    pairs
}

impl Settings {
    /// `load_settings()`: the process environment over `.env` in the working directory.
    pub fn load() -> Result<Settings, ConfigurationError> {
        let dotenv = std::fs::read_to_string(Path::new(".env")).ok();
        let env: Vec<(String, String)> = std::env::vars().collect();
        Settings::from_sources(&env, dotenv.as_deref())
    }

    /// Settings from environment variables over the text of a `.env` file. A variable is matched to its setting
    /// without regard to case, as pydantic-settings matches it.
    pub fn from_sources<K: AsRef<str>, V: AsRef<str>>(
        env: &[(K, V)],
        dotenv: Option<&str>,
    ) -> Result<Settings, ConfigurationError> {
        let mut values: HashMap<String, String> = HashMap::new();
        for (key, value) in dotenv.map(parse_dotenv).unwrap_or_default() {
            values.insert(key.to_lowercase(), value);
        }
        for (key, value) in env {
            values.insert(key.as_ref().to_lowercase(), value.as_ref().to_string());
        }
        let mut settings = Settings::default();
        let mut invalid = Vec::new();
        for (name, variable) in FIELDS {
            let former = FORMERLY.iter().find(|(new, _)| *new == variable).map(|(_, old)| *old);
            let Some(value) = values.get(&variable.to_lowercase()).or_else(|| former.and_then(|old| values.get(old)))
            else {
                continue;
            };
            if settings.assign(name, value).is_err() {
                invalid.push(name);
            }
        }
        // Only the variable the named provider reads: a key for any other provider stays out of the settings.
        if let Some(KeySource::Variable(variable)) = settings.llm_provider.map(|provider| provider.preset().key) {
            settings.llm_provider_api_key = values.get(&variable.to_lowercase()).map(Secret::new);
        }
        if !invalid.is_empty() {
            // Name the setting, never echo the input: the field may be a key.
            return Err(ConfigurationError(format!("invalid settings: {}", invalid.join(", "))));
        }
        Ok(settings)
    }

    fn assign(&mut self, name: &str, value: &str) -> Result<(), ()> {
        let text = || Some(value.to_string());
        let secret = || Some(Secret::new(value));
        match name {
            "TYPESAFE_API_KEY" => self.typesafe_api_key = secret(),
            "AI_GATEWAY_API_KEY" => self.ai_gateway_api_key = secret(),
            "OPENROUTER_API_KEY" => self.openrouter_api_key = secret(),
            "BROWSER_USE_API_KEY" => self.browser_use_api_key = secret(),
            "mcp_token" => self.mcp_token = secret(),
            "decision_source" => self.decision_source = Some(DecisionSource::parse(value).ok_or(())?),
            "decision_base_url" => self.decision_base_url = text(),
            "decision_model" => self.decision_model = text(),
            "decision_api_key" => self.decision_api_key = secret(),
            "TELNYX_API_KEY" => self.telnyx_api_key = secret(),
            "OPENAI_API_KEY" => self.openai_api_key = secret(),
            "AIMLAPI_API_KEY" => self.aimlapi_api_key = secret(),
            "PERPLEXITY_API_KEY" => self.perplexity_api_key = secret(),
            "llm_provider" => self.llm_provider = Some(LLMProvider::parse(value).ok_or(())?),
            "llm_model" => self.llm_model = text(),
            "llm_model_plan" => self.llm_model_plan = text(),
            "llm_model_read" => self.llm_model_read = text(),
            "llm_model_field_text" => self.llm_model_field_text = text(),
            "llm_model_recover" => self.llm_model_recover = text(),
            "llm_model_compose" => self.llm_model_compose = text(),
            "llm_model_verify" => self.llm_model_verify = text(),
            "llm_model_shortcut" => self.llm_model_shortcut = text(),
            "llm_base_url" => self.llm_base_url = text(),
            "llm_api_key" => self.llm_api_key = secret(),
            "llm_strict" => {
                self.llm_strict = bool_of(&Value::String(value.into())).map_err(|_| ())?;
                self.llm_strict_set = true;
            }
            "llm_extra_body" => self.llm_extra_body = text(),
            "llm_reasoning" => {
                self.llm_reasoning = ReasoningEffort::parse(value).ok_or(())?;
                self.llm_reasoning_set = true;
            }
            "llm_reasoning_plan" => self.llm_reasoning_plan = Some(ReasoningEffort::parse(value).ok_or(())?),
            "llm_reasoning_read" => self.llm_reasoning_read = Some(ReasoningEffort::parse(value).ok_or(())?),
            "llm_reasoning_field_text" => {
                self.llm_reasoning_field_text = Some(ReasoningEffort::parse(value).ok_or(())?)
            }
            "llm_reasoning_recover" => self.llm_reasoning_recover = Some(ReasoningEffort::parse(value).ok_or(())?),
            "llm_reasoning_compose" => self.llm_reasoning_compose = Some(ReasoningEffort::parse(value).ok_or(())?),
            "llm_reasoning_verify" => self.llm_reasoning_verify = Some(ReasoningEffort::parse(value).ok_or(())?),
            "llm_reasoning_shortcut" => self.llm_reasoning_shortcut = Some(ReasoningEffort::parse(value).ok_or(())?),
            "chrome" => self.chrome = text(),
            "headed" => self.headed = bool_of(&Value::String(value.into())).map_err(|_| ())?,
            "profile" => self.profile = Some(PathBuf::from(value)),
            _ => {}
        }
        Ok(())
    }

    /// Per-purpose models, most specific setting winning.
    pub fn models(&self) -> HashMap<LLMPurpose, String> {
        LLMPurpose::ALL
            .iter()
            .map(|purpose| {
                let model = [self.purpose_model(*purpose), self.llm_model.as_ref()]
                    .into_iter()
                    .flatten()
                    .find(|model| !model.is_empty())
                    .cloned()
                    .unwrap_or_else(|| default_model(*purpose).to_string());
                (*purpose, model)
            })
            .collect()
    }

    fn purpose_model(&self, purpose: LLMPurpose) -> Option<&String> {
        match purpose {
            LLMPurpose::Plan => self.llm_model_plan.as_ref(),
            LLMPurpose::Read => self.llm_model_read.as_ref(),
            LLMPurpose::FieldText => self.llm_model_field_text.as_ref(),
            LLMPurpose::Recover => self.llm_model_recover.as_ref(),
            LLMPurpose::Compose => self.llm_model_compose.as_ref(),
            LLMPurpose::Verify => self.llm_model_verify.as_ref(),
            LLMPurpose::Shortcut => self.llm_model_shortcut.as_ref(),
        }
    }

    /// The purposes whose reasoning effort is set for that purpose alone.
    fn purpose_reasoning(&self) -> HashMap<LLMPurpose, ReasoningEffort> {
        LLMPurpose::ALL
            .iter()
            .filter_map(|purpose| {
                let effort = match purpose {
                    LLMPurpose::Plan => self.llm_reasoning_plan,
                    LLMPurpose::Read => self.llm_reasoning_read,
                    LLMPurpose::FieldText => self.llm_reasoning_field_text,
                    LLMPurpose::Recover => self.llm_reasoning_recover,
                    LLMPurpose::Compose => self.llm_reasoning_compose,
                    LLMPurpose::Verify => self.llm_reasoning_verify,
                    LLMPurpose::Shortcut => self.llm_reasoning_shortcut,
                };
                effort.map(|effort| (*purpose, effort))
            })
            .collect()
    }

    pub fn local_chrome(&self) -> LocalChrome {
        LocalChrome { binary: self.chrome.clone(), headed: self.headed, profile: self.profile.clone() }
    }

    /// The Jev provider a run starts on, and the one it fails over to when that one's retries run out.
    pub fn decision_route(&self) -> (DecisionSource, Option<DecisionSource>) {
        let source = self.decision_source.unwrap_or(if set(&self.typesafe_api_key) {
            DecisionSource::TypeSafe
        } else if set(&self.openrouter_api_key) {
            DecisionSource::OpenRouter
        } else {
            DecisionSource::Gateway
        });
        // A proxy may be a routing boundary, and the gateway cannot honour a direct-API model pin.
        if !source.serves_jev()
            || self.decision_base_url.as_ref().is_some_and(|url| !url.is_empty())
            || self.decision_model.is_some()
        {
            return (source, None);
        }
        let backup = match source {
            DecisionSource::OpenRouter | DecisionSource::TypeSafe => {
                set(&self.ai_gateway_api_key).then_some(DecisionSource::Gateway)
            }
            DecisionSource::Gateway if set(&self.typesafe_api_key) => Some(DecisionSource::TypeSafe),
            DecisionSource::Gateway => set(&self.openrouter_api_key).then_some(DecisionSource::OpenRouter),
            _ => None,
        };
        (source, backup)
    }

    /// Which providers a run will call, for the first line of a log: an outage reads differently with no backup.
    pub fn providers(&self) -> String {
        let (source, backup) = self.decision_route();
        let failover = match backup {
            Some(backup) => format!("backup {backup}"),
            None => "no backup: an outage past its retries ends the run".to_string(),
        };
        let llm = self.llm_source();
        let models = match llm {
            LLMProvider::OpenRouter | LLMProvider::Gateway => Ok(self.models()),
            _ => self.preset_models(&llm.preset()),
        };
        let models = match models {
            Ok(models) => models.into_values().collect::<BTreeSet<String>>().into_iter().collect::<Vec<_>>().join(", "),
            Err(_) => "model unset".to_string(),
        };
        format!("Jev {source} ({failover}); LLM {models} via {}", llm.label())
    }

    /// Where the LLM goes: the provider FASTBROWSE_LLM_PROVIDER names; otherwise a custom endpoint when
    /// FASTBROWSE_LLM_BASE_URL is set, else OpenRouter when keyed, otherwise the gateway, which serves the same models.
    pub fn llm_source(&self) -> LLMProvider {
        if let Some(provider) = self.llm_provider {
            return provider;
        }
        if self.llm_base_url().is_some() {
            return LLMProvider::Custom;
        }
        if set(&self.openrouter_api_key) { LLMProvider::OpenRouter } else { LLMProvider::Gateway }
    }

    /// An empty FASTBROWSE_LLM_BASE_URL= left in a `.env` is unset, not a request to an empty address.
    fn llm_base_url(&self) -> Option<&str> {
        self.llm_base_url.as_deref().map(str::trim).filter(|url| !url.is_empty())
    }

    pub fn decision(&self, http: &Http) -> Result<Box<dyn DecisionClient>, ConfigurationError> {
        let (source, backup_source) = self.decision_route();
        let primary = self.decision_client(source, http)?;
        let Some(backup_source) = backup_source else { return Ok(primary) };
        Ok(Box::new(FailoverDecisionClient::new(primary, self.decision_client(backup_source, http)?, http.clone())))
    }

    fn decision_client(
        &self,
        source: DecisionSource,
        http: &Http,
    ) -> Result<Box<dyn DecisionClient>, ConfigurationError> {
        let base_url = |default: &'static str| match &self.decision_base_url {
            Some(url) if !url.is_empty() => url.clone(),
            _ => default.to_string(),
        };
        let model = |default: &'static str| match &self.decision_model {
            Some(model) if !model.is_empty() => model.clone(),
            _ => default.to_string(),
        };
        let key = |key: &Option<Secret>, message: &str| match key {
            Some(key) if key.is_set() => Ok(key.expose().to_string()),
            _ => Err(ConfigurationError(message.to_string())),
        };
        Ok(match source {
            DecisionSource::OpenRouter => Box::new(SystemOneClient::new(
                key(&self.openrouter_api_key, "set OPENROUTER_API_KEY for Jev via OpenRouter")?,
                http.clone(),
                &base_url(OPENROUTER_URL),
                &model(OPENROUTER_MODEL),
            )),
            DecisionSource::TypeSafe => Box::new(SystemOneClient::new(
                key(&self.typesafe_api_key, "set TYPESAFE_API_KEY for Jev via the direct API")?,
                http.clone(),
                &base_url(TYPESAFE_URL),
                &model(JEV_MODEL),
            )),
            DecisionSource::Gateway => Box::new(VercelGatewayJevClient::new(
                key(
                    &self.ai_gateway_api_key,
                    "set AI_GATEWAY_API_KEY for Jev via the gateway, OPENROUTER_API_KEY or TYPESAFE_API_KEY",
                )?,
                http.clone(),
                &base_url(GATEWAY_URL),
            )),
            DecisionSource::Telnyx => Box::new(SystemOneClient::new(
                key(&self.telnyx_api_key, "set TELNYX_API_KEY for decisions via Telnyx")?,
                http.clone(),
                &base_url(TELNYX_URL),
                &model(TELNYX_MODEL),
            )),
            DecisionSource::OpenAI => Box::new(OpenAIDecisionsClient::new(
                key(&self.openai_api_key, "set OPENAI_API_KEY for decisions via OpenAI")?,
                http.clone(),
                &base_url(OPENAI_URL),
                &model(OPENAI_DECISIONS_MODEL),
            )),
            DecisionSource::Aimlapi => {
                let key = key(&self.aimlapi_api_key, "set AIMLAPI_API_KEY for decisions via AIMLAPI")?;
                let model = model(AIMLAPI_MODEL);
                // AIMLAPI takes gpt-6-luna in OpenAI's own shape and every other model in System One's.
                if model.rsplit('/').next() == Some(OPENAI_DECISIONS_MODEL) {
                    Box::new(OpenAIDecisionsClient::new(key, http.clone(), &base_url(AIMLAPI_URL), &model).unpriced())
                } else {
                    let client = SystemOneClient::new(key, http.clone(), &base_url(AIMLAPI_URL), &model);
                    Box::new(client.at(DECISIONS_PATH).unpriced())
                }
            }
            DecisionSource::Perplexity => Box::new(
                SystemOneClient::new(
                    key(&self.perplexity_api_key, "set PERPLEXITY_API_KEY for decisions via Perplexity")?,
                    http.clone(),
                    &base_url(PERPLEXITY_URL),
                    &model(PERPLEXITY_MODEL),
                )
                .at(DECISIONS_PATH),
            ),
            // Only this source's own key: a TypeSafe, OpenRouter or gateway key must never reach another host.
            DecisionSource::Ollama | DecisionSource::Custom => {
                let base_url = match (source, &self.decision_base_url) {
                    (_, Some(url)) if !url.is_empty() => url.clone(),
                    (DecisionSource::Ollama, _) => OLLAMA_URL.to_string(),
                    _ => {
                        return Err(ConfigurationError(
                            "set FASTBROWSE_DECISION_BASE_URL to the System One host for the custom source".into(),
                        ));
                    }
                };
                let Some(model) = self.decision_model.as_deref().map(str::trim).filter(|model| !model.is_empty())
                else {
                    return Err(ConfigurationError(format!(
                        "set FASTBROWSE_DECISION_MODEL to the decision model the {source} source serves"
                    )));
                };
                let key = self.decision_api_key.as_ref().map(|key| key.expose().trim().to_string()).unwrap_or_default();
                Box::new(SystemOneClient::new(key, http.clone(), &base_url, model))
            }
        })
    }

    /// The extra request body, parsed. A malformed value is a configuration error, not a retry.
    fn extra_body(&self) -> Result<Map<String, Value>, ConfigurationError> {
        let Some(raw) = self.llm_extra_body.as_ref().filter(|raw| !raw.is_empty()) else { return Ok(Map::new()) };
        let parsed: Value = serde_json::from_str(raw)
            .map_err(|_| ConfigurationError("FASTBROWSE_LLM_EXTRA_BODY is not valid JSON".into()))?;
        let Value::Object(parsed) = parsed else {
            return Err(ConfigurationError("FASTBROWSE_LLM_EXTRA_BODY must be a JSON object".into()));
        };
        let clashes = reserved_clashes(&parsed);
        if !clashes.is_empty() {
            return Err(ConfigurationError(format!(
                "FASTBROWSE_LLM_EXTRA_BODY cannot set {}: fastbrowse sets those fields itself",
                clashes.join(", ")
            )));
        }
        Ok(parsed)
    }

    /// Per-purpose models for a preset, falling back only on the preset's own defaults.
    ///
    /// The OpenRouter default is an id another provider would not serve, which surfaces as a 404 on the first plan
    /// rather than as the setting that is missing; a blank `FASTBROWSE_LLM_MODEL=` in a `.env` is unset too.
    fn preset_models(&self, preset: &Preset) -> Result<HashMap<LLMPurpose, String>, ConfigurationError> {
        LLMPurpose::ALL
            .iter()
            .map(|purpose| {
                [self.purpose_model(*purpose), self.llm_model.as_ref()]
                    .into_iter()
                    .flatten()
                    .map(|model| model.trim())
                    .find(|model| !model.is_empty())
                    .or_else(|| preset.models.map(|default| default(*purpose)))
                    .map(|model| (*purpose, model.to_string()))
                    .ok_or_else(|| {
                        ConfigurationError(match preset.provider {
                            LLMProvider::Custom => {
                                "set FASTBROWSE_LLM_MODEL to the model FASTBROWSE_LLM_BASE_URL serves".into()
                            }
                            provider => format!("set FASTBROWSE_LLM_MODEL to a model {} serves", provider.label()),
                        })
                    })
            })
            .collect()
    }

    /// The address a preset's requests go to. A provider's key is sent to that provider's host alone, so a hosted
    /// preset refuses another address rather than carry its key there.
    fn preset_url(&self, preset: &Preset) -> Result<String, ConfigurationError> {
        let provider = preset.provider;
        match (preset.endpoint, self.llm_base_url()) {
            (Endpoint::Hosted(url), None) => Ok(url.to_string()),
            (Endpoint::Hosted(_), Some(_)) => Err(ConfigurationError(format!(
                "FASTBROWSE_LLM_BASE_URL cannot be used with FASTBROWSE_LLM_PROVIDER={provider}, whose key goes to its \
                 own host only; use FASTBROWSE_LLM_PROVIDER=custom for another endpoint"
            ))),
            (Endpoint::Resource(suffixes), Some(url)) => {
                let parsed = url::Url::parse(url).ok();
                let host = parsed.as_ref().filter(|url| url.scheme() == "https").and_then(url::Url::host_str);
                match host {
                    Some(host) if suffixes.iter().any(|suffix| host.ends_with(suffix)) => Ok(url.to_string()),
                    _ => Err(ConfigurationError(format!(
                        "FASTBROWSE_LLM_BASE_URL must be an https address on {} for FASTBROWSE_LLM_PROVIDER={provider}",
                        suffixes.join(" or ")
                    ))),
                }
            }
            (Endpoint::Resource(suffixes), None) => Err(ConfigurationError(format!(
                "set FASTBROWSE_LLM_BASE_URL to your resource's /openai/v1 address on {} for \
                 FASTBROWSE_LLM_PROVIDER={provider}",
                suffixes.join(" or ")
            ))),
            (Endpoint::Local(_) | Endpoint::Custom, Some(url)) => Ok(url.to_string()),
            (Endpoint::Local(url), None) => Ok(url.to_string()),
            (Endpoint::Custom, None) => {
                Err(ConfigurationError("set FASTBROWSE_LLM_BASE_URL for FASTBROWSE_LLM_PROVIDER=custom".into()))
            }
        }
    }

    fn preset_llm(&self, preset: Preset, http: &Http) -> Result<Box<dyn LLMClient>, ConfigurationError> {
        let provider = preset.provider;
        let base_url = self.preset_url(&preset)?;
        let key = match preset.key {
            // Only this provider's own key: an OpenRouter or gateway key must never reach another host.
            KeySource::Local => {
                self.llm_api_key.as_ref().map(|key| key.expose().trim().to_string()).unwrap_or_default()
            }
            KeySource::Variable(variable) => match &self.llm_provider_api_key {
                Some(key) if !key.expose().trim().is_empty() => key.expose().trim().to_string(),
                _ => {
                    return Err(ConfigurationError(format!("set {variable} for FASTBROWSE_LLM_PROVIDER={provider}")));
                }
            },
        };
        if preset.wire == Wire::AnthropicMessages {
            // Claude thinks by default, and on a short output cap a default effort can spend the cap before the
            // answer, so the effort is always sent, low unless FASTBROWSE_LLM_REASONING says otherwise.
            let models = self.preset_models(&preset)?;
            return AnthropicLLM::new(
                key,
                http.clone(),
                &base_url,
                models,
                Some(self.llm_reasoning),
                self.extra_body()?,
            )
            .map(|llm| Box::new(llm) as Box<dyn LLMClient>)
            .map_err(ConfigurationError);
        }
        // The effort goes only where it was asked for: a strict server answers HTTP 400 to a field it does not know,
        // and each provider's own default is a better guess than OpenRouter's for its models.
        let purpose_reasoning = self.purpose_reasoning();
        let reasoning_effort = match (self.llm_reasoning_set || !purpose_reasoning.is_empty(), preset.reasoning) {
            (false, _) => None,
            (true, Some(_)) => self.llm_reasoning_set.then_some(self.llm_reasoning),
            (true, None) => {
                return Err(ConfigurationError(format!(
                    "{} takes no reasoning effort through its OpenAI-compatible API; unset FASTBROWSE_LLM_REASONING \
                     and FASTBROWSE_LLM_REASONING_<PURPOSE>",
                    provider.label()
                )));
            }
        };
        let options = LLMOptions {
            reasoning_effort,
            purpose_reasoning,
            reasoning_field: preset.reasoning.unwrap_or(ReasoningField::OpenRouter),
            provider_routing: None,
            strict: if self.llm_strict_set { self.llm_strict } else { preset.strict },
            output_format: preset.output_format,
            output_cap_field: preset.output_cap_field,
            auth_header: preset.auth_header,
            extra_body: self.extra_body()?,
        };
        OpenAICompatibleLLM::new(key, http.clone(), &base_url, self.preset_models(&preset)?, options)
            .map(|llm| Box::new(llm) as Box<dyn LLMClient>)
            .map_err(ConfigurationError)
    }

    pub fn llm(&self, http: &Http) -> Result<Box<dyn LLMClient>, ConfigurationError> {
        let source = self.llm_source();
        if matches!(source, LLMProvider::OpenRouter | LLMProvider::Gateway)
            && self.llm_provider.is_some()
            && self.llm_base_url().is_some()
        {
            return Err(ConfigurationError(format!(
                "FASTBROWSE_LLM_BASE_URL cannot be used with FASTBROWSE_LLM_PROVIDER={source}, whose key goes to its own \
                 host only; use FASTBROWSE_LLM_PROVIDER=custom for another endpoint"
            )));
        }
        let (key, base_url, routing) = match source {
            LLMProvider::OpenRouter => match &self.openrouter_api_key {
                Some(key) if key.is_set() => (key, LLM_OPENROUTER_URL, openrouter_provider_routing()),
                _ => return Err(ConfigurationError("set OPENROUTER_API_KEY or AI_GATEWAY_API_KEY for the LLM".into())),
            },
            LLMProvider::Gateway => match &self.ai_gateway_api_key {
                Some(key) if key.is_set() => (key, LLM_GATEWAY_URL, gateway_provider_routing()),
                _ => return Err(ConfigurationError("set AI_GATEWAY_API_KEY or OPENROUTER_API_KEY for the LLM".into())),
            },
            provider => return self.preset_llm(provider.preset(), http),
        };
        let options = LLMOptions {
            reasoning_effort: Some(self.llm_reasoning),
            purpose_reasoning: self.purpose_reasoning(),
            provider_routing: Some(routing),
            ..LLMOptions::default()
        };
        OpenAICompatibleLLM::new(key.expose(), http.clone(), base_url, self.models(), options)
            .map(|llm| Box::new(llm) as Box<dyn LLMClient>)
            .map_err(ConfigurationError)
    }

    pub fn openrouter_key(&self) -> Result<&str, ConfigurationError> {
        match &self.openrouter_api_key {
            Some(key) if key.is_set() => Ok(key.expose()),
            _ => Err(ConfigurationError("set OPENROUTER_API_KEY for the LLM".into())),
        }
    }

    pub fn browser_key(&self) -> Result<&str, ConfigurationError> {
        match &self.browser_use_api_key {
            Some(key) if key.is_set() => Ok(key.expose()),
            _ => Err(ConfigurationError("set BROWSER_USE_API_KEY for a cloud browser".into())),
        }
    }
}
