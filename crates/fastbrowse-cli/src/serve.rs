//! Serve runs to another process over JSON-RPC on stdio.
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
use fastbrowse_core::models::{
    Event, EventHandler, FrameHandler, Image, RunResult, SecretRef, SecretResolver, UntilCheck,
};
use fastbrowse_core::protocol::{
    Error, ErrorCode, ErrorResponse, InitializeParams, InitializeResult, JsonRpc, Method, NoParams, Notification,
    PROTOCOL_VERSION, Request, RequestId, Response, RunCancelParams, RunEvent, RunFrame, RunParams, RunUntilParams,
    SecretsResolveParams, ServerMethod, ServerRequest,
};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::text::repr;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::log;
use crate::options;
use crate::runner::{Cancel, Host, RunError, RunTask, Runner};

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
        let payload: Value = match serde_json::from_slice(line) {
            Ok(payload) => payload,
            Err(error) => {
                let error = Error { code: ErrorCode::ParseError, message: error.to_string() };
                return self.write(&ErrorResponse { jsonrpc: JsonRpc, id: None, error }).await;
            }
        };
        if let Value::Object(map) = &payload
            && !map.contains_key("method")
            && (map.contains_key("result") || map.contains_key("error"))
        {
            return self.settle(map);
        }
        let request: Request = match serde_json::from_value(payload.clone()) {
            Ok(request) => request,
            Err(error) => {
                let error = Error { code: ErrorCode::InvalidRequest, message: error.to_string() };
                return self.write(&ErrorResponse { jsonrpc: JsonRpc, id: id_of(&payload), error }).await;
            }
        };
        let id = request.id;
        let params = match payload.get("params") {
            None | Some(Value::Null) => json!({}),
            Some(params) => params.clone(),
        };
        let Some(known) = Method::parse(&request.method) else {
            let refusal = refused(ErrorCode::MethodNotFound, format!("unknown method {}", repr(&request.method)));
            return self.refuse(id.as_ref(), refusal).await;
        };
        if params.is_array() {
            let message = format!("{}: params: name them; no method takes them by position", known.as_str());
            return self.refuse(id.as_ref(), refused(ErrorCode::InvalidParams, message)).await;
        }
        let checked = match known {
            Method::Initialize => read::<InitializeParams>(known, params.clone()).map(drop),
            Method::Shutdown => read::<NoParams>(known, params.clone()).map(drop),
            Method::RunCancel | Method::Run => Ok(()),
        };
        if let Err(refusal) = checked {
            return self.refuse(id.as_ref(), refusal).await;
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
            Method::RunCancel => match read::<RunCancelParams>(known, params) {
                Ok(cancel) => {
                    if self.state().active.as_deref() == Some(cancel.run_id.as_str()) {
                        self.cancel_run();
                    }
                    self.respond(id.as_ref(), Value::Null).await;
                }
                Err(refusal) => self.refuse(id.as_ref(), refusal).await,
            },
            Method::Run => match read::<RunParams>(known, params) {
                Ok(params) => self.start_run(id, params).await,
                Err(refusal) => self.refuse(id.as_ref(), refusal).await,
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
    /// Everything up to the run's first wait happens here, before the next line is read: a
    /// `run/cancel` or the end of input on the next line finds the run under way.
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
        let run_id = params.run_id.clone();
        let arguments = match self.arguments(params, &context) {
            Ok(arguments) => arguments,
            Err(refusal) => {
                self.finish_run();
                return self.refuse(id.as_ref(), refusal).await;
            }
        };
        let server = self.clone();
        let mut running: BoxFuture<'static, ()> = Box::pin(async move {
            let (task, host) = arguments;
            let outcome = server.0.runner.run(task, host).await;
            server.finish_run();
            let failure = context.failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            match reply(outcome, failure, &run_id) {
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

    /// The callbacks a run of this server reaches its client through, and the run's arguments with them.
    fn arguments(&self, params: RunParams, context: &RunContext) -> Result<(RunTask, Host), Refused> {
        let (events, frames, checks, secrets) = (context.clone(), context.clone(), context.clone(), context.clone());
        let callbacks = Callbacks {
            cancel: context.cancel.clone(),
            on_event: Arc::new(move |event: Event| {
                let context = events.clone();
                Box::pin(async move {
                    // The run waits for this before it goes on, which puts the events on the wire in order and ahead
                    // of the reply.
                    let params = RunEvent { run_id: context.run_id.clone(), event };
                    context.server.notify(ServerMethod::RunEvent, params.dump()).await;
                })
            }),
            on_frame: Some(Arc::new(move |frame: Vec<u8>| {
                let context = frames.clone();
                Box::pin(async move {
                    let params = RunFrame { run_id: context.run_id.clone(), frame: Image(frame) };
                    context.server.notify(ServerMethod::RunFrame, params.dump()).await;
                })
            })),
            until: Some(Arc::new(move |url: String| {
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
            })),
            resolve: Some(Arc::new(move |name: String, origin: String| {
                let context = secrets.clone();
                Box::pin(async move {
                    let failure = secret_failure(&name);
                    let question = SecretsResolveParams { run_id: context.run_id.clone(), name, origin };
                    match context.ask(ServerMethod::SecretsResolve, question.dump(), Some(failure)).await? {
                        Ok(answer) => secret_value(answer).unwrap_or_else(|failure| context.fail(failure)),
                        Err(failure) => context.fail(failure),
                    }
                })
            })),
        };
        arguments(params, &self.0.settings, callbacks)
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

/// A `run` request as the run and what hosts it, or the reason it cannot become a run.
///
/// What the environment can get wrong is found here, before a browser opens: the run reads its model keys only when
/// it builds the clients, and a missing Chrome would surface as a failed run. The checks hold for a stand-in runner
/// too, so every entry point refuses the same requests whatever runs them.
pub fn arguments(
    params: RunParams,
    settings: &SettingsSource,
    callbacks: Callbacks,
) -> Result<(RunTask, Host), Refused> {
    let RunParams { secrets, frames, until, run: mut task, .. } = params;
    let settings = settings()?;
    if !task.fetch {
        let local = task.chrome.clone().unwrap_or_default();
        task.chrome = Some(options::chrome(&settings, local.headed, local.profile.as_deref(), local.binary.as_deref()));
    }
    fastbrowse_run::check(&task).map_err(|error| match error {
        RunError::UnsupportedSchema(message) => refused(ErrorCode::UnsupportedSchema, message),
        RunError::Configuration(message) => refused(ErrorCode::Configuration, message),
        other => refused(ErrorCode::InvalidParams, other.to_string()),
    })?;
    let browser_api_key = options::environment(&task, &settings)?;
    let host = Host {
        browser_api_key,
        secrets: match callbacks.resolve {
            Some(resolve) if !secrets.is_empty() => Some(Arc::new(ClientSecrets { refs: secrets, resolve }) as _),
            _ => None,
        },
        cancel: callbacks.cancel,
        on_event: Some(callbacks.on_event),
        // A run with no frame handler starts no screencast, so a client that reads no frames pays for none.
        on_frame: callbacks.on_frame.filter(|_| frames),
        until: callbacks.until.filter(|_| until),
        ..Host::default()
    };
    Ok((task, host))
}

/// What a run's callbacks reach, wherever the client is: over the wire for `serve`, in this process for the bindings.
pub struct Callbacks {
    pub cancel: Cancel,
    pub on_event: EventHandler,
    /// Used only by a run whose params set `frames`.
    pub on_frame: Option<FrameHandler>,
    /// Used only by a run whose params set `until`.
    pub until: Option<UntilCheck>,
    /// Asked for a secret's value once the origin rule has let the question through.
    pub resolve: Option<Resolve>,
}

/// `(name, origin)` to the value the client holds, or None.
pub type Resolve = Arc<dyn Fn(String, String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// A method's params as its model, or the refusal that names the method, the field at fault and the fault.
pub fn read<T: DeserializeOwned>(method: Method, params: Value) -> Result<T, Refused> {
    serde_path_to_error::deserialize(params).map_err(|error| {
        let message = match error.path().to_string() {
            path if path == "." => error.into_inner().to_string(),
            path => format!("{path}: {}", error.into_inner()),
        };
        refused(ErrorCode::InvalidParams, format!("{}: {message}", method.as_str()))
    })
}

/// How a run's outcome is answered. `failure` is the first callback the client failed, which stopped the run.
pub fn reply(
    outcome: Result<RunResult, RunError>,
    failure: Option<ClientError>,
    run_id: &str,
) -> Result<RunResult, Refused> {
    match outcome {
        Ok(result) => Ok(result),
        Err(RunError::Configuration(message)) => Err(refused(ErrorCode::Configuration, message)),
        // A callback's failure stopped the run, and the reply says what the client failed. What the
        // run did before then is in the events.
        Err(RunError::Cancelled) => match failure {
            Some(failure) => Ok(options::error_result(failure.to_string())),
            None => Err(refused(ErrorCode::Cancelled, format!("run {} was cancelled", repr(run_id)))),
        },
        Err(RunError::UnsupportedSchema(message)) => Err(refused(ErrorCode::UnsupportedSchema, message)),
        Err(RunError::Invalid(message) | RunError::Recording(message)) => {
            log::error(&format!("serve: run {} failed: {message}", repr(run_id)));
            Ok(options::error_result(message))
        }
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

/// The client's answer to `run/until`, named by what was expected and never by what came.
fn strict_bool(value: Value) -> Result<bool, ClientError> {
    value.as_bool().ok_or_else(|| ClientError {
        method: ServerMethod::RunUntil,
        message: "the result cannot be used: expected true or false".into(),
        code: None,
    })
}

/// What a failed `secrets/resolve` says in place of the client's message. A client's own account of why it has no
/// value may quote the value, and the run's result is read by people and models the value is kept from.
pub fn secret_failure(name: &str) -> String {
    format!("the client could not resolve the secret {}", repr(name))
}

/// The client's answer to `secrets/resolve`: a string, or null for none.
pub fn secret_value(answer: Value) -> Result<Option<String>, ClientError> {
    match answer {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value)),
        _ => Err(ClientError {
            method: ServerMethod::SecretsResolve,
            message: "the result cannot be used: expected a string or null".into(),
            code: None,
        }),
    }
}

/// A run's `SecretResolver` for values the client holds. Each one is asked for as it is typed, and none is kept. The
/// run applies each ref's origin rule before it asks.
struct ClientSecrets {
    refs: Vec<SecretRef>,
    resolve: Resolve,
}

impl SecretResolver for ClientSecrets {
    fn available(&self) -> Vec<SecretRef> {
        self.refs.clone()
    }

    fn resolve<'a>(&'a self, name: &'a str, origin: &'a str) -> BoxFuture<'a, Option<String>> {
        (self.resolve)(name.to_string(), origin.to_string())
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
