//! The model clients, ported from `src/fastbrowse/clients`: Jev through TypeSafe, OpenRouter or the Vercel AI Gateway,
//! the LLM through any OpenAI-compatible chat completions endpoint, and the settings that choose between them.
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
