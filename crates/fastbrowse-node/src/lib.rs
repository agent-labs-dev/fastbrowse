//! The Rust core in-process for Node, loaded as a `.node` addon.
//!
//! napi converts a JS string to UTF-8 with U+FFFD for a lone surrogate, as WebIDL does for a USVString.

use std::collections::HashMap;
use std::sync::Arc;

use fastbrowse_cli::embed::{Embedded, Host as EmbeddedHost, HostCall, HostResolve};
use fastbrowse_cli::runner::Cancel;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::{citations, origins};
use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;
use napi_derive::napi;
use serde_json::{Value, json};

/// A host function the run awaits. It answers with JSON, `{"ok": value}` or `{"error": message}`, and never throws or
/// rejects, so what the caller's function threw reaches the run as its message and nothing else.
type HostFn<T> = ThreadsafeFunction<T, Promise<String>, T, Status, false>;

/// The callbacks of one run. Each stands for the `serve` message of the same purpose.
#[napi(object, object_to_js = false)]
pub struct RunHost {
    /// `run/event`: called with the event as JSON.
    pub on_event: Option<HostFn<String>>,
    /// `run/frame`: called with one JPEG. Needed when the params set `frames`.
    pub on_frame: Option<HostFn<Buffer>>,
    /// `run/until`: called with the final URL, answers a boolean. Needed when the params set `until`.
    pub until: Option<HostFn<String>>,
    /// `secrets/resolve`: called with `[name, origin]` as JSON, answers a string or null. Needed when the params name
    /// secrets.
    pub resolve_secret: Option<HostFn<String>>,
}

/// Stops the run it is handed to, as an `AbortSignal` would: the run closes its browser, then its promise settles
/// with the cancelled error.
#[napi]
pub struct RunCancel {
    inner: Cancel,
}

#[napi]
impl RunCancel {
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        RunCancel { inner: Cancel::default() }
    }

    #[napi]
    pub fn cancel(&self) {
        self.inner.trip();
    }
}

fn answer(reply: Result<String>) -> std::result::Result<Value, String> {
    let reply = reply.map_err(|error| error.reason.clone())?;
    let mut reply: Value = serde_json::from_str(&reply).map_err(|error| error.to_string())?;
    if let Some(Value::String(message)) = reply.get("error") {
        return Err(message.clone());
    }
    Ok(reply.get_mut("ok").map(Value::take).unwrap_or(Value::Null))
}

fn host_call<T, R>(function: HostFn<T>, read: fn(Value) -> std::result::Result<R, String>) -> HostCall<T, R>
where
    T: JsValuesTupleIntoVec + Send + 'static,
    R: Send + 'static,
{
    let function = Arc::new(function);
    Arc::new(move |value: T| {
        let function = function.clone();
        Box::pin(async move {
            let reply = match function.call_async_catch(value).await {
                Ok(promise) => promise.await,
                Err(error) => Err(error),
            };
            answer(reply).and_then(read)
        })
    })
}

fn nothing(_: Value) -> std::result::Result<(), String> {
    Ok(())
}

fn boolean(value: Value) -> std::result::Result<bool, String> {
    value.as_bool().ok_or_else(|| "the result cannot be used: Input should be a valid boolean".into())
}

fn secret(value: Value) -> std::result::Result<Option<String>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value)),
        _ => Err("the result cannot be used: Input should be a valid string".into()),
    }
}

impl RunHost {
    fn embedded(self) -> EmbeddedHost {
        let resolve_secret = self.resolve_secret.map(|function| -> HostResolve {
            let call = host_call(function, secret);
            Arc::new(move |name: String, origin: String| call(json!([name, origin]).to_string()))
        });
        let on_event = self.on_event.map(|function| {
            let call = host_call(function, nothing);
            Arc::new(move |event: Value| call(event.to_string())) as HostCall<Value, ()>
        });
        let on_frame = self.on_frame.map(|function| {
            let call = host_call(function, nothing);
            Arc::new(move |frame: Vec<u8>| call(Buffer::from(frame))) as HostCall<Vec<u8>, ()>
        });
        EmbeddedHost {
            on_event,
            on_frame,
            until: self.until.map(|function| host_call(function, boolean)),
            resolve_secret,
        }
    }
}

/// `{"result": RunResult}`, or `{"error": {"code", "message"}}` for a run refused before it began: a JSON-RPC
/// reply without its id.
fn reply(outcome: std::result::Result<fastbrowse_core::models::RunResult, fastbrowse_cli::serve::Refused>) -> String {
    match outcome {
        Ok(result) => json!({"result": result.dump()}).to_string(),
        Err(refused) => json!({"error": {"code": refused.code.code(), "message": refused.message}}).to_string(),
    }
}

/// Runs that share settings: the process environment with `env` over it, then `.env`. One per `Fastbrowse` instance
/// of the SDK, as one `serve` process is.
#[napi]
pub struct Core {
    embedded: Arc<Embedded>,
}

#[napi]
impl Core {
    #[napi(constructor)]
    pub fn new(env: Option<HashMap<String, String>>) -> Self {
        Core { embedded: Arc::new(Embedded::new(env.unwrap_or_default().into_iter().collect())) }
    }

    /// Run the task `params_json` describes, a `run` request's params as JSON. Resolves with the reply as JSON.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_task<'env>(
        &self,
        env: &'env Env,
        params_json: String,
        host: Option<RunHost>,
        cancel: Option<&RunCancel>,
    ) -> Result<PromiseRaw<'env, String>> {
        start(env, self.embedded.clone(), params_json, host, cancel)
    }
}

fn start<'env>(
    env: &'env Env,
    embedded: Arc<Embedded>,
    params_json: String,
    host: Option<RunHost>,
    cancel: Option<&RunCancel>,
) -> Result<PromiseRaw<'env, String>> {
    let host = host.map(RunHost::embedded).unwrap_or_default();
    let cancel = cancel.map(|cancel| cancel.inner.clone()).unwrap_or_default();
    env.spawn_future(async move { Ok(reply(embedded.run(&params_json, host, cancel).await)) })
}

/// `Core` with the process environment alone, for one run.
#[napi(ts_return_type = "Promise<string>")]
pub fn run_task<'env>(
    env: &'env Env,
    params_json: String,
    host: Option<RunHost>,
    cancel: Option<&RunCancel>,
) -> Result<PromiseRaw<'env, String>> {
    start(env, Arc::new(Embedded::new(Vec::new())), params_json, host, cancel)
}

/// The version of the core and of the protocol its params and replies follow.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[napi]
pub fn protocol_version() -> i64 {
    fastbrowse_core::protocol::PROTOCOL_VERSION
}

#[napi]
pub fn origin_of(url: String) -> Option<String> {
    origins::origin_of(&url)
}

#[napi]
pub fn parse_origin(value: String) -> Result<String> {
    origins::parse_origin(&value).map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
}

#[napi]
pub fn parse_origins(values: Vec<String>) -> Result<Vec<String>> {
    origins::parse_origins(values).map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
}

#[napi]
pub fn text_fragment(url: String, text: String) -> Result<String> {
    citations::text_fragment(&url, &text).map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
}

#[napi]
pub struct OriginGrant {
    inner: origins::OriginGrant,
}

#[napi]
impl OriginGrant {
    #[napi(constructor)]
    pub fn new(origins: Vec<String>) -> Self {
        Self { inner: origins::OriginGrant::new(origins) }
    }

    #[napi]
    pub fn allows(&self, url: String) -> bool {
        self.inner.allows(&url)
    }
}
