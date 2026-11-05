//! The model clients: Jev through TypeSafe, OpenRouter or the Vercel AI Gateway, other decision models through Telnyx,
//! Ollama, AIMLAPI, Perplexity, OpenAI or a custom host, the LLM through any OpenAI-compatible chat completions endpoint
//! or Anthropic's Messages API, and the settings that choose between them.
//!
//! Every request body is written as httpx writes `json=`, so a replay keyed on a request's path and canonical body finds
//! the response Python recorded. Timeouts and retry delays are Python's constants, read on tokio's clock
//! so a test can pause it.

pub mod anthropic;
pub mod environment;
pub mod failover;
pub mod http;
pub mod openai_compatible;
pub mod openai_decisions;
pub mod providers;
pub mod system_one;
pub mod validation;
pub mod vercel;
