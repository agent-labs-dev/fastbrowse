//! Port of `src/fastbrowse/serve.py`: serve runs to another process over JSON-RPC on stdio.
//!
//! ```text
//! fastbrowse serve --stdio
//! ```
//!
//! The fourth entry point, beside the CLI, the MCP server and `run_task`. It is what the JavaScript SDK starts and
//! talks to. The messages are in `fastbrowse_core::protocol`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use fastbrowse_clients::environment::{ConfigurationError, Settings};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{Attachment, Bytes, Event, Image, SecretRef, SecretResolver};
use fastbrowse_core::output_schema::output_model;
use fastbrowse_core::protocol::{
    Error, ErrorCode, ErrorResponse, InitializeResult, JsonRpc, Method, Notification, PROTOCOL_VERSION, RequestId,
    Response, RunCancelParams, RunEvent, RunFrame, RunParams, RunUntilParams, SecretsResolveParams, ServerMethod,
    ServerRequest,
};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::text::repr;
use fastbrowse_core::safety::secret_allowed;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::options::{self, HandOver};
use crate::runner::{Cancel, RunArgs, RunError, Runner};
use crate::{log, pyjson, validate};

/// Lines in and lines out. `receive` returns one line, or None once the input has ended. `send` writes one whole
/// line before it returns, so lines go out in the order they were sent and never interleave.
pub trait Transport: Send + Sync + 'static {
    fn receive(&self) -> BoxFuture<'_, Option<Vec<u8>>>;

    fn send(&self, line: &[u8]);
}

/// The process's own stdin and stdout, as lines of any length.
///
/// A thread doing blocking reads hands each line over as it ends: a `run` carrying attachments is one line of tens of
/// megabytes, and its pieces are joined once, when its end comes. Nothing else in this process writes to stdout; a
/// program it starts (Chrome, ffmpeg) is given stderr in its place.
pub struct StdioTransport {
    lines: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl StdioTransport {
    pub fn start() -> StdioTransport {
        StdioTransport::reading(std::io::stdin())
    }

    pub fn reading(mut input: impl Read + Send + 'static) -> StdioTransport {
        let (sender, lines) = mpsc::unbounded_channel();
        // Detached, because after `shutdown` it is still blocked on input that may never come.
        std::thread::spawn(move || {
            let mut pending: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 1 << 16];
            loop {
                let read = match input.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let mut rest = &chunk[..read];
                while let Some(end) = rest.iter().position(|b| *b == b'\n') {
                    pending.extend_from_slice(&rest[..end]);
                    if sender.send(std::mem::take(&mut pending)).is_err() {
                        return;
                    }
                    rest = &rest[end + 1..];
                }
                pending.extend_from_slice(rest);
            }
            if !pending.is_empty() {
                let _ = sender.send(pending);
            }
        });
        StdioTransport { lines: tokio::sync::Mutex::new(lines) }
    }
}

impl Transport for StdioTransport {
    fn receive(&self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move { self.lines.lock().await.recv().await })
    }

    fn send(&self, line: &[u8]) {
        let mut stdout = std::io::stdout().lock();
        // A client that closed its end gets nothing more; the input ending is what stops the server.
        let _ = stdout.write_all(line).and_then(|()| stdout.flush());
    }
}

/// A request that gets an error reply, raised wherever the reason is found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub code: ErrorCode,
    pub message: String,
}

fn refused(code: ErrorCode, message: impl Into<String>) -> Refused {
    Refused { code, message: message.into() }
}

impl From<ConfigurationError> for Refused {
    fn from(error: ConfigurationError) -> Refused {
        refused(ErrorCode::Configuration, error.0)
    }
}

/// The client failed one of the server's requests: an error reply, or a result of the wrong shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientError {
    pub method: ServerMethod,
    pub message: String,
    /// The code of the client's error reply. None when the client replied with a result that could not be used.
    pub code: Option<i64>,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.method, self.message)
    }
}

/// One of the server's requests that the client has not answered yet.
struct Question {
    method: ServerMethod,
    answer: oneshot::Sender<Result<Value, ClientError>>,
    /// What to say when the client answers with an error, in place of the message it sent.
    failure: Option<String>,
}

pub type SettingsSource = Arc<dyn Fn() -> Result<Settings, ConfigurationError> + Send + Sync>;

#[derive(Default)]
struct State {
    closing: bool,
    // One run at a time: a caller that wants more starts more processes.
    active: Option<String>,
    running: Option<Cancel>,
    answering: Vec<JoinHandle<()>>,
    last_request_id: i64,
    pending: HashMap<i64, Question>,
}

struct Inner {
    transport: Arc<dyn Transport>,
    runner: Arc<dyn Runner>,
    settings: SettingsSource,
    protocol_version: i64,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct Server(Arc<Inner>);

/// A run's own callbacks share this: the first callback the client fails ends the run with that failure.
#[derive(Clone)]
struct RunContext {
    server: Server,
    run_id: String,
    cancel: Cancel,
    failure: Arc<Mutex<Option<ClientError>>>,
}

impl Server {
    pub fn new(transport: Arc<dyn Transport>, runner: Arc<dyn Runner>, settings: SettingsSource) -> Server {
        Server::with_protocol(transport, runner, settings, PROTOCOL_VERSION)
    }

    /// A server that says it speaks another protocol version, for a client's test of a mismatch.
    pub fn with_protocol(
        transport: Arc<dyn Transport>,
        runner: Arc<dyn Runner>,
        settings: SettingsSource,
        protocol_version: i64,
    ) -> Server {
        Server(Arc::new(Inner { transport, runner, settings, protocol_version, state: Mutex::default() }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Answer requests until the client is done, and return the process's exit code.
    ///
    /// The client is done when it says `shutdown` or when the input ends. A parent that was killed says nothing, and
    /// its end of the pipe closing is the only notice there is.
    pub async fn serve(&self) -> i32 {
        while !self.state().closing {
            let Some(line) = self.0.transport.receive().await else { break };
            self.receive(&line).await;
        }
        // A run outlives the loop that started it. Stopping it closes its browser, and the process stays until that
        // is done and the run is answered.
        self.cancel_run();
        let answering = std::mem::take(&mut self.state().answering);
        for task in answering {
            let _ = task.await;
        }
        0
    }

    async fn receive(&self, line: &[u8]) {
        let payload = match pyjson::loads(line) {
            Ok(payload) => payload,
            Err(message) => {
                let error = Error { code: ErrorCode::ParseError, message };
                return self.write(&ErrorResponse { jsonrpc: JsonRpc, id: None, error }).await;
            }
        };
        if let Value::Object(map) = &payload
            && !map.contains_key("method")
            && (map.contains_key("result") || map.contains_key("error"))
        {
            return self.settle(map);
        }
        if let Err(problems) = validate::check(&validate::request(), &payload) {
            let error = Error { code: ErrorCode::InvalidRequest, message: problems };
            return self.write(&ErrorResponse { jsonrpc: JsonRpc, id: id_of(&payload), error }).await;
        }
        let id = id_of(&payload);
        let method = payload["method"].as_str().unwrap_or_default().to_string();
        let params = match payload.get("params") {
            None | Some(Value::Null) => json!({}),
            Some(params) => params.clone(),
        };
        let Some(known) = Method::parse(&method) else {
            let refusal = refused(ErrorCode::MethodNotFound, format!("unknown method {}", repr(&method)));
            return self.refuse(id.as_ref(), refusal).await;
        };
        let shape = match known {
            Method::Initialize => validate::initialize_params(),
            Method::Run => validate::run_params(),
            Method::RunCancel => validate::run_cancel_params(),
            Method::Shutdown => validate::no_params(),
        };
        if let Err(problems) = validate::check(&shape, &params) {
            return self.refuse(id.as_ref(), refused(ErrorCode::InvalidParams, format!("{method}: {problems}"))).await;
        }
        match known {
            Method::Initialize => {
                let result = InitializeResult {
                    protocol_version: self.0.protocol_version,
                    fastbrowse_version: env!("CARGO_PKG_VERSION").to_string(),
                };
                self.respond(id.as_ref(), result.dump()).await;
            }
            Method::Shutdown => {
                self.state().closing = true;
                self.respond(id.as_ref(), Value::Null).await;
            }
            Method::RunCancel => {
                match serde_json::from_value::<RunCancelParams>(params) {
                    Ok(cancel) if self.state().active.as_deref() == Some(cancel.run_id.as_str()) => self.cancel_run(),
                    _ => {}
                }
                self.respond(id.as_ref(), Value::Null).await;
            }
            Method::Run => match serde_json::from_value::<RunParams>(params) {
                Ok(params) => self.start_run(id, params).await,
                Err(error) => {
                    let refusal = refused(ErrorCode::InvalidParams, format!("{method}: params: {error}"));
                    self.refuse(id.as_ref(), refusal).await;
                }
            },
        }
    }

    async fn respond(&self, id: Option<&RequestId>, result: Value) {
        if let Some(id) = id {
            self.write(&Response { jsonrpc: JsonRpc, id: id.clone(), result }).await;
        }
    }

    async fn refuse(&self, id: Option<&RequestId>, refusal: Refused) {
        // Named in the log as well as the reply, since a notification that was refused has no reply.
        log::warning(&format!("serve: {}", refusal.message));
        if let Some(id) = id {
            let error = Error { code: refusal.code, message: refusal.message };
            self.write(&ErrorResponse { jsonrpc: JsonRpc, id: Some(id.clone()), error }).await;
        }
    }

    /// Start a run, and answer it from a task of its own so the server keeps reading while it goes on.
    ///
    /// Everything up to the run's first wait happens here, before the next line is read, as Python starts the task
    /// eagerly: a `run/cancel` or the end of input on the next line finds the run under way.
    async fn start_run(&self, id: Option<RequestId>, params: RunParams) {
        let cancel = Cancel::default();
        let busy = {
            let mut state = self.state();
            match &state.active {
                Some(active) => Some(format!("run {} is still active, and a server runs one at a time", repr(active))),
                None => {
                    state.active = Some(params.run_id.clone());
                    state.running = Some(cancel.clone());
                    None
                }
            }
        };
        if let Some(message) = busy {
            return self.refuse(id.as_ref(), refused(ErrorCode::Busy, message)).await;
        }
        let context = RunContext {
            server: self.clone(),
            run_id: params.run_id.clone(),
            cancel: cancel.clone(),
            failure: Arc::default(),
        };
        let arguments = match self.arguments(&params, &context) {
            Ok(arguments) => arguments,
            Err(refusal) => {
                self.finish_run();
                return self.refuse(id.as_ref(), refusal).await;
            }
        };
        let server = self.clone();
        let mut running: BoxFuture<'static, ()> = Box::pin(async move {
            let outcome = server.0.runner.run(arguments).await;
            server.finish_run();
            let failure = context.failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            let reply = match outcome {
                Ok(result) => Ok(result),
                Err(RunError::Configuration(message)) => Err(refused(ErrorCode::Configuration, message)),
                // A callback's failure stopped the run, and the reply says what the client failed, as Python's
                // `run_task` lets the callback's exception through. What the run did before then is in the events.
                Err(RunError::Cancelled) => match failure {
                    Some(failure) => Ok(options::error_result(failure.to_string())),
                    None => Err(refused(ErrorCode::Cancelled, format!("run {} was cancelled", repr(&context.run_id)))),
                },
                Err(RunError::Failed(message)) => {
                    // The reply has room for what went wrong and none for where, so the detail goes to the log.
                    log::error(&format!("serve: run {} failed\n{message}", repr(&context.run_id)));
                    Ok(options::error_result(message))
                }
            };
            match reply {
                Ok(result) => server.respond(id.as_ref(), result.dump()).await,
                Err(refusal) => server.refuse(id.as_ref(), refusal).await,
            }
        });
        // One poll here, the rest from a task: the run gets as far as its first wait before the next line is read.
        if std::future::poll_fn(|context| std::task::Poll::Ready(running.as_mut().poll(context))).await.is_pending() {
            let task = tokio::spawn(running);
            self.state().answering.push(task);
        }
    }

    fn finish_run(&self) {
        let mut state = self.state();
        state.active = None;
        state.running = None;
    }

    fn cancel_run(&self) {
        // Once per run. A second stop would land in the run while it is closing the browser.
        let running = self.state().running.take();
        if let Some(cancel) = running {
            cancel.trip();
        }
    }

    /// A `run` request as `run_task`'s arguments, or the reason it cannot become a run.
    ///
    /// What the environment can get wrong is found here, before a browser opens: `run_task` reads its model keys only
    /// when it builds the clients, and a missing Chrome would surface as a failed run. The key checks hold for a
    /// stand-in runner too, so the server refuses the same requests whatever runs them.
    fn arguments(&self, params: &RunParams, context: &RunContext) -> Result<RunArgs, Refused> {
        let proxy_country = match &params.proxy_country {
            None => None,
            Some(asked) => Some(
                options::country_code(asked)
                    .map_err(|error| refused(ErrorCode::InvalidParams, format!("run: proxy_country: {error}")))?,
            ),
        };
        let output_schema = match &params.output_schema {
            None => None,
            Some(schema) => Some(
                output_model(&Value::Object(schema.clone()))
                    .map_err(|error| refused(ErrorCode::UnsupportedSchema, format!("run: output_schema: {error}")))?,
            ),
        };
        options::recording(params.record.as_deref())?;
        let settings = (self.0.settings)()?;
        let local = params.chrome.clone().unwrap_or_default();
        let chrome = options::chrome(&settings, local.headed, local.profile.as_deref(), local.binary.as_deref());
        let handed_over = options::handed_over(&HandOver {
            cdp_url: params.cdp_url.as_deref(),
            cdp_port: params.cdp_port,
            attach: params.attach,
            target_match: params.target_match.as_deref(),
            local: params.local,
            cloud_asked: false,
            chrome: &chrome,
            cloud_profile: params.cloud_profile.as_deref(),
            proxy_country: proxy_country.as_deref(),
        })?;
        let on_cloud = !handed_over
            && options::cloud(params.local, &chrome, params.cloud_profile.as_deref(), proxy_country.as_deref())?;
        if !handed_over && !on_cloud && options::find_chrome(chrome.binary.as_deref()).is_none() {
            return Err(ConfigurationError(
                "Chrome was not found: install it, or name it in FASTBROWSE_CHROME or in `chrome.binary`".into(),
            )
            .into());
        }
        let browser_api_key = options::browser_key(&settings, on_cloud)?;
        options::check_model_keys(&settings)?;
        let mut args = RunArgs::new(params.task.clone());
        args.start = params.start.clone();
        args.browser_api_key = browser_api_key;
        args.chrome = Some(chrome);
        args.cloud_profile = params.cloud_profile.clone();
        args.cdp_url = params.cdp_url.clone();
        args.cdp_port = params.cdp_port;
        args.attach = params.attach;
        args.target_match = params.target_match.clone();
        args.foreground = params.foreground;
        args.proxy_country = Some(options::proxy_country(proxy_country.as_deref()));
        args.viewport = params.viewport;
        args.cloud_allow_resizing = params.cloud_allow_resizing;
        args.inputs = params.inputs.clone();
        args.attachments = params
            .attachments
            .iter()
            .map(|attachment| Attachment {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                content: Bytes(attachment.content.0.clone()),
            })
            .collect();
        args.limits = params.limits.clone();
        args.authorization = params.authorization.clone();
        args.output_schema = output_schema;
        args.secrets = (!params.secrets.is_empty())
            .then(|| Arc::new(ClientSecrets { refs: params.secrets.clone(), context: context.clone() }) as _);
        args.downloads = params.downloads.clone();
        args.record = params.record.clone();
        args.cursor = params.cursor;
        args.cancel = context.cancel.clone();
        let events = context.clone();
        args.on_event = Some(Arc::new(move |event: Event| {
            let context = events.clone();
            Box::pin(async move {
                // The run waits for this before it goes on, which puts the events on the wire in order and ahead of
                // the reply.
                let params = RunEvent { run_id: context.run_id.clone(), event };
                context.server.notify(ServerMethod::RunEvent, params.dump()).await;
            })
        }));
        // A run with no frame handler starts no screencast, so a client that reads no frames pays for none.
        if params.frames {
            let frames = context.clone();
            args.on_frame = Some(Arc::new(move |frame: Vec<u8>| {
                let context = frames.clone();
                Box::pin(async move {
                    let params = RunFrame { run_id: context.run_id.clone(), frame: Image(frame) };
                    context.server.notify(ServerMethod::RunFrame, params.dump()).await;
                })
            }));
        }
        if params.until {
            let checks = context.clone();
            args.until = Some(Arc::new(move |url: String| {
                let context = checks.clone();
                Box::pin(async move {
                    let question = RunUntilParams { run_id: context.run_id.clone(), url };
                    let answer = context.ask(ServerMethod::RunUntil, question.dump(), None).await;
                    match answer.map(|answer| answer.and_then(strict_bool)) {
                        Some(Ok(passed)) => passed,
                        Some(Err(failure)) => context.fail(failure),
                        None => false,
                    }
                })
            }));
        }
        Ok(args)
    }

    async fn notify(&self, method: ServerMethod, params: Value) {
        self.write(&Notification { jsonrpc: JsonRpc, method, params }).await;
    }

    /// Hand a reply from the client to the request that is waiting for it.
    ///
    /// Nothing of the reply is logged, and nothing is written back: its result may be a secret's value, and JSON-RPC
    /// has no answer to an answer.
    fn settle(&self, reply: &serde_json::Map<String, Value>) {
        // The server's own ids are integers, so no other id answers anything.
        let Some(id) = reply.get("id").and_then(|id| fastbrowse_core::py::lax::strict_int_of(id).ok()) else {
            return;
        };
        let error = match reply.get("error") {
            None | Some(Value::Null) => None,
            // A reply that falls short of `protocol.Error` still has to end the run waiting on it, so a member of the
            // wrong type is read as absent and anything that is not an object as an error with no members.
            Some(Value::Object(error)) => Some((
                error.get("code").and_then(|code| fastbrowse_core::py::lax::strict_int_of(code).ok()),
                error.get("message").and_then(Value::as_str).map(str::to_string),
            )),
            Some(_) => Some((None, None)),
        };
        // A run that was stopped while its question was out leaves the answer with nobody to take it.
        let Some(question) = self.state().pending.remove(&id) else { return };
        let answer = match error {
            None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
            Some((code, message)) => Err(ClientError {
                method: question.method,
                message: question.failure.or(message).unwrap_or_else(|| "the client replied with an error".to_string()),
                code,
            }),
        };
        let _ = question.answer.send(answer);
    }

    async fn write(&self, message: &impl Serialize) {
        let mut line = message.dump_json().into_bytes();
        line.push(b'\n');
        self.0.transport.send(&line);
        // Python hands each write to a thread and waits for it, which lets the other tasks run; so does this.
        tokio::task::yield_now().await;
    }
}

impl RunContext {
    /// Ask the client, and wait for its answer as long as it takes, or until the run is stopped (None).
    ///
    /// There is no timeout here. What is waiting is a run, and its `max_seconds` already bounds it.
    async fn ask(
        &self,
        method: ServerMethod,
        params: Value,
        failure: Option<String>,
    ) -> Option<Result<Value, ClientError>> {
        let (sender, answer) = oneshot::channel();
        let id = {
            let mut state = self.server.state();
            state.last_request_id += 1;
            let id = state.last_request_id;
            state.pending.insert(id, Question { method, answer: sender, failure });
            id
        };
        self.server.write(&ServerRequest { jsonrpc: JsonRpc, id, method, params }).await;
        let outcome = self.cancel.guard(answer).await;
        self.server.state().pending.remove(&id);
        match outcome {
            Ok(Ok(answer)) => Some(answer),
            _ => None,
        }
    }

    /// End the run on a callback the client failed: the failure is kept for the reply, and the run is stopped.
    fn fail<T: Default>(&self, failure: ClientError) -> T {
        self.failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert(failure);
        self.cancel.trip();
        T::default()
    }
}

/// `TypeAdapter(bool).validate_python(answer, strict=True)`, named by what was expected and never by what came.
fn strict_bool(value: Value) -> Result<bool, ClientError> {
    value.as_bool().ok_or_else(|| ClientError {
        method: ServerMethod::RunUntil,
        message: "the result cannot be used: Input should be a valid boolean".into(),
        code: None,
    })
}

/// A run's `SecretResolver` for values the client holds. Each one is asked for as it is typed, and none is kept.
///
/// The origin rule is applied here, ahead of the question: a client asked about an origin its ref does not cover
/// would have to apply the rule a second time to refuse, and the two could disagree.
struct ClientSecrets {
    refs: Vec<SecretRef>,
    context: RunContext,
}

impl SecretResolver for ClientSecrets {
    fn available(&self) -> Vec<SecretRef> {
        self.refs.clone()
    }

    fn resolve<'a>(&'a self, name: &'a str, origin: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let secret = self.refs.iter().find(|secret| secret.name == name)?;
            if !secret_allowed(secret, origin) {
                return None;
            }
            let question = SecretsResolveParams {
                run_id: self.context.run_id.clone(),
                name: name.to_string(),
                origin: origin.to_string(),
            };
            // A client's own account of why it has no value may quote the value, and the run's result is read by
            // people and models the value is kept from.
            let failure = format!("the client could not resolve the secret {}", repr(name));
            match self.context.ask(ServerMethod::SecretsResolve, question.dump(), Some(failure)).await? {
                Ok(Value::Null) => None,
                Ok(Value::String(value)) => Some(value),
                Ok(_) => self.context.fail(ClientError {
                    method: ServerMethod::SecretsResolve,
                    message: "the result cannot be used: Input should be a valid string".into(),
                    code: None,
                }),
                Err(failure) => self.context.fail(failure),
            }
        })
    }
}

/// The id to answer a request that could not be read as one, when it at least had a usable id.
fn id_of(payload: &Value) -> Option<RequestId> {
    match payload.get("id")? {
        Value::String(id) => Some(RequestId::Str(id.clone())),
        id => id.as_i64().map(RequestId::Int),
    }
}

/// The settings a run is checked against: the environment, then `.env`.
pub fn load_settings() -> SettingsSource {
    Arc::new(Settings::load)
}
