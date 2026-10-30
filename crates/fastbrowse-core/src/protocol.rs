//! The messages `fastbrowse serve` exchanges with its client, and the version
//! of that exchange.
//!
//! JSON-RPC 2.0, one message per line, each written as `model_dump_json()` writes it.

use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::models::{Authorization, Event, Image, Limits, LocalChrome, RunResult, SecretRef};
use crate::py::{base64, lax};
use crate::schema::{Field, Mode, ModelTy, Schema, Ty, definitions};

/// Changes only when the wire format does, so it is not the package version: a release that leaves the messages alone
/// leaves this alone, and an SDK can tell a binary it cannot talk to from one that is merely newer.
pub const PROTOCOL_VERSION: i64 = 1;

/// `StrictInt | StrictStr`. Strict, because JSON-RPC echoes the id back and a `true` read as 1 would answer a request
/// nobody sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

impl Serialize for RequestId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RequestId::Int(id) => serializer.serialize_i64(*id),
            RequestId::Str(id) => serializer.serialize_str(id),
        }
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(id) => Ok(RequestId::Str(id)),
            value => lax::strict_int_of(&value).map(RequestId::Int).map_err(serde::de::Error::custom),
        }
    }
}

str_enum! {
    /// What a client can ask of the server.
    doc: "What a client can ask of the server.";
    pub enum Method {
        Initialize = "initialize",
        Run = "run",
        RunCancel = "run/cancel",
        Shutdown = "shutdown",
    }
}

str_enum! {
    /// What the server sends without being asked.
    doc: "What the server sends without being asked.";
    pub enum ServerMethod {
        RunEvent = "run/event",
        RunFrame = "run/frame",
        RunUntil = "run/until",
        SecretsResolve = "secrets/resolve",
    }
}

/// Every code an error reply can carry: the JSON-RPC standard ones, then this protocol's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    // JSON-RPC reserves -32000 to -32099 for the server's own errors.
    Busy,
    Cancelled,
    Configuration,
    UnsupportedSchema,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 9] = [
        ErrorCode::ParseError,
        ErrorCode::InvalidRequest,
        ErrorCode::MethodNotFound,
        ErrorCode::InvalidParams,
        ErrorCode::InternalError,
        ErrorCode::Busy,
        ErrorCode::Cancelled,
        ErrorCode::Configuration,
        ErrorCode::UnsupportedSchema,
    ];

    pub fn code(self) -> i64 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            ErrorCode::Busy => -32001,
            ErrorCode::Cancelled => -32002,
            ErrorCode::Configuration => -32003,
            ErrorCode::UnsupportedSchema => -32004,
        }
    }

    pub fn from_code(code: i64) -> Option<ErrorCode> {
        ErrorCode::ALL.into_iter().find(|c| c.code() == code)
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.code())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let code = lax::int(d)?;
        ErrorCode::from_code(code).ok_or_else(|| serde::de::Error::custom(format!("{code} is not a known error code")))
    }
}

literal_tag!(JsonRpc = "2.0");

/// `dict[str, Any] | list[Any] | None`. JSON-RPC also allows params by position. No method here takes them, and
/// accepting the shape lets the refusal be "invalid params" for that method instead of "invalid request".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Params {
    Named(serde_json::Map<String, Value>),
    Positional(Vec<Value>),
}

/// A call in either direction. One without an id is a notification and gets no reply. Unknown members are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: JsonRpc,
    #[serde(default)]
    pub id: Option<RequestId>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Params>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub jsonrpc: JsonRpc,
    pub id: RequestId,
    /// `Any`, required. A missing member is refused like any other missing field.
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    #[serde(default)]
    pub jsonrpc: JsonRpc,
    /// Null when the request could not be read far enough to learn its id.
    #[serde(deserialize_with = "lax::nullable")]
    pub id: Option<RequestId>,
    pub error: Error,
}

/// The params of `initialize` and `shutdown`: none, and an unknown one refused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

/// The client's side of the handshake.
///
/// Unknown fields are allowed here and nowhere else: a client on a later protocol may send some, and it has to get
/// the server's version back to report the mismatch, which a refusal of its params would hide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(deserialize_with = "lax::strict_int")]
    pub protocol_version: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(deserialize_with = "lax::int")]
    pub protocol_version: i64,
    pub fastbrowse_version: String,
}

/// Bytes that travel as base64: read with `b64decode`'s leniency, written in the standard alphabet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Base64Bytes(pub Vec<u8>);

impl Serialize for Base64Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Base64Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        base64::decode(&text)
            .map(Base64Bytes)
            .map_err(|error| serde::de::Error::custom(format!("Base64 decoding error: '{error}'")))
    }
}

/// An `Attachment` as it travels: JSON has no bytes, so the content is base64.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunAttachment {
    pub name: String,
    pub mime_type: String,
    pub content: Base64Bytes,
}

fn run_id<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let id = String::deserialize(d)?;
    if id.is_empty() {
        return Err(serde::de::Error::custom("run_id must not be empty"));
    }
    Ok(id)
}

/// One run's arguments: what the CLI's flags, a `run` request, the MCP `browse` tool and the Python and Node `run_task`
/// all describe. The caller's own objects (its callbacks, its model clients, a browser key) are not among them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTask {
    pub task: String,
    #[serde(default)]
    pub start: Option<String>,
    /// Background the run may use and never reads as the task, such as where the request came from.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub inputs: Option<IndexMap<String, String>>,
    #[serde(default)]
    pub attachments: Vec<RunAttachment>,
    #[serde(default)]
    pub limits: Option<Limits>,
    #[serde(default)]
    pub authorization: Option<Authorization>,
    /// The shape of `RunResult.data`, as JSON Schema draft 2020-12 with an object at its root.
    #[serde(default)]
    pub output_schema: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub downloads: Option<PathBuf>,
    #[serde(default)]
    pub record: Option<PathBuf>,
    /// Draw the agent's cursor over a visible local or attached Chrome with the Cua Driver.
    #[serde(default)]
    pub cursor: bool,
    /// Local Chrome instead of a Browser Use Cloud browser.
    #[serde(default)]
    pub local: bool,
    /// Which local Chrome, and how. A headed window or a kept profile implies `local`.
    #[serde(default)]
    pub chrome: Option<LocalChrome>,
    #[serde(default)]
    pub cloud_profile: Option<String>,
    #[serde(default)]
    pub cdp_url: Option<String>,
    #[serde(default)]
    pub cdp_port: Option<u16>,
    #[serde(default)]
    pub attach: bool,
    #[serde(default)]
    pub target_match: Option<String>,
    /// Bring the run's tab and its browser window to the front, to watch it.
    #[serde(default)]
    pub foreground: bool,
    /// The two-letter country a cloud browser browses from; the United States when none is given.
    #[serde(default)]
    pub proxy_country: Option<String>,
    #[serde(default)]
    pub viewport: Option<(u32, u32)>,
    #[serde(default)]
    pub cloud_extensions: Vec<String>,
    /// How long a cloud browser may live; 15 minutes when none is given.
    #[serde(default)]
    pub cloud_timeout_minutes: Option<u32>,
    /// The only origins the run may open, each exact.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
    /// Read every page over HTTP and start no browser.
    #[serde(default)]
    pub fetch: bool,
    /// Read over HTTP first, and start a browser only if the task turns out to need one.
    #[serde(default)]
    pub fetch_first: bool,
}

/// One run: the run's arguments, and what the server needs to talk to the client about it.
///
/// The reply is the `RunResult`. No API key is among the fields: model and browser keys come from the environment
/// the server inherited.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunParams {
    /// Chosen by the client, and carried by every message the server sends about this run.
    pub run_id: String,
    /// The secrets the client holds, by name and origin. A value is asked for with `secrets/resolve`.
    pub secrets: Vec<SecretRef>,
    /// Send the active tab as `run/frame` notifications while the run goes on. Off, the page is never filmed.
    pub frames: bool,
    /// Whether the client holds a check of its own. The check stays there: the server asks it with `run/until`.
    pub until: bool,
    #[serde(flatten)]
    pub run: RunTask,
}

/// The fields of `RunParams` that are about the exchange rather than the run.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    #[serde(deserialize_with = "run_id")]
    run_id: String,
    #[serde(default)]
    secrets: Vec<SecretRef>,
    #[serde(default)]
    frames: bool,
    #[serde(default)]
    until: bool,
}

/// `value` read as `T`, its error naming the field at fault, since the caller's own path ends at this map.
fn located<T: serde::de::DeserializeOwned, E: serde::de::Error>(value: Value) -> Result<T, E> {
    serde_path_to_error::deserialize(value).map_err(|error| match error.path().to_string() {
        path if path == "." => E::custom(error.into_inner()),
        path => E::custom(format!("{path}: {}", error.into_inner())),
    })
}

impl<'de> Deserialize<'de> for RunParams {
    /// Split by hand, since a flattened struct accepts any field: a misspelled option must be refused, not dropped.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut run = serde_json::Map::<String, Value>::deserialize(d)?;
        let envelope: serde_json::Map<String, Value> = ["run_id", "secrets", "frames", "until"]
            .into_iter()
            .filter_map(|key| run.remove(key).map(|value| (key.to_string(), value)))
            .collect();
        let envelope: Envelope = located(Value::Object(envelope))?;
        let run: RunTask = located(Value::Object(run))?;
        Ok(RunParams {
            run_id: envelope.run_id,
            secrets: envelope.secrets,
            frames: envelope.frames,
            until: envelope.until,
            run,
        })
    }
}

/// Which run to stop. One that is not active, because it finished or never was, is left as it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelParams {
    pub run_id: String,
}

/// The params of `run/event`: one thing a run did, written before that run's reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: String,
    pub event: Event,
}

/// The params of `run/frame`: the active tab as it looked a moment ago. Sent only to a run that set `frames`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFrame {
    pub run_id: String,
    /// One JPEG.
    pub frame: Image,
}

/// A message from the server that takes no reply. It has no `id` member at all, which is what marks it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub jsonrpc: JsonRpc,
    pub method: ServerMethod,
    pub params: Value,
}

/// A call from the server that the client answers, with a `Response` or an `ErrorResponse` carrying its id.
///
/// The ids are the server's own and count up from one. A reply is told from a request by having no `method`, so a
/// client's ids may be the same numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerRequest {
    #[serde(default)]
    pub jsonrpc: JsonRpc,
    #[serde(deserialize_with = "lax::int")]
    pub id: i64,
    pub method: ServerMethod,
    pub params: Value,
}

/// The params of `secrets/resolve`: one value the run is about to type. The reply is the value, or null.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretsResolveParams {
    pub run_id: String,
    pub name: String,
    /// Where it will be typed. The server has already checked it against the origins the ref declared.
    pub origin: String,
}

/// The params of `run/until`: the address a run ended on. The reply is a boolean, and false keeps the run from
/// `complete`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunUntilParams {
    pub run_id: String,
    pub url: String,
}

/// A model of this module that ignores a field it does not know. Its fields have no descriptions: Python's protocol
/// models never read attribute docstrings, as `Frozen` does.
fn message(name: &str, doc: Option<&str>, fields: Vec<Field>) -> ModelTy {
    let model = ModelTy { name: name.into(), module: "fastbrowse.protocol", doc: None, forbid_extra: false, fields };
    match doc {
        Some(doc) => model.doc(doc),
        None => model,
    }
}

/// A method's params, which refuse a field they do not know.
fn params(name: &str, doc: Option<&str>, fields: Vec<Field>) -> ModelTy {
    ModelTy { forbid_extra: true, ..message(name, doc, fields) }
}

fn jsonrpc() -> Field {
    Field::default("jsonrpc", JsonRpc::ty(), "2.0")
}

impl Schema for RequestId {
    fn ty() -> Ty {
        Ty::Alias("RequestId", Box::new(Ty::Union(vec![Ty::Int, Ty::Str])))
    }
}

impl Schema for ErrorCode {
    fn ty() -> Ty {
        Ty::Enum(std::sync::Arc::new(crate::schema::EnumTy {
            name: "ErrorCode",
            doc: Some("Every code an error reply can carry: the JSON-RPC standard ones, then this protocol's own."),
            values: ErrorCode::ALL.iter().map(|code| code.code().into()).collect(),
        }))
    }
}

impl Schema for Request {
    fn ty() -> Ty {
        let positional = Ty::Union(vec![Ty::map(Ty::Any), Ty::list(Ty::Any)]);
        message(
            "Request",
            Some("A call in either direction. One without an id is a notification and gets no reply."),
            vec![
                jsonrpc(),
                Field::default("id", Ty::optional(RequestId::ty()), Value::Null),
                Field::required("method", Ty::Str),
                Field::default("params", Ty::optional(positional), Value::Null),
            ],
        )
        .ty()
    }
}

impl Schema for Response {
    fn ty() -> Ty {
        message(
            "Response",
            None,
            vec![jsonrpc(), Field::required("id", RequestId::ty()), Field::required("result", Ty::Any)],
        )
        .ty()
    }
}

impl Schema for Error {
    fn ty() -> Ty {
        message("Error", None, vec![Field::required("code", ErrorCode::ty()), Field::required("message", Ty::Str)]).ty()
    }
}

impl Schema for ErrorResponse {
    fn ty() -> Ty {
        message(
            "ErrorResponse",
            None,
            vec![
                jsonrpc(),
                Field::required("id", Ty::optional(RequestId::ty())),
                Field::required("error", Error::ty()),
            ],
        )
        .ty()
    }
}

impl Schema for NoParams {
    fn ty() -> Ty {
        params("NoParams", None, vec![]).ty()
    }
}

impl Schema for InitializeParams {
    fn ty() -> Ty {
        message(
            "InitializeParams",
            Some(
                "The client's side of the handshake.\n\nUnknown fields are allowed here and nowhere else: a client on a later protocol may send some, and it has to\nget the server's version back to report the mismatch, which a refusal of its params would hide.",
            ),
            vec![Field::required("protocol_version", Ty::Int)],
        )
        .ty()
    }
}

impl Schema for InitializeResult {
    fn ty() -> Ty {
        message(
            "InitializeResult",
            None,
            vec![Field::required("protocol_version", Ty::Int), Field::required("fastbrowse_version", Ty::Str)],
        )
        .ty()
    }
}

impl Schema for RunAttachment {
    fn ty() -> Ty {
        params(
            "RunAttachment",
            Some("An `Attachment` as it travels: JSON has no bytes, so the content is base64."),
            vec![
                Field::required("name", Ty::Str),
                Field::required("mime_type", Ty::Str),
                Field::required("content", Ty::Base64),
            ],
        )
        .ty()
    }
}

/// `RunTask`'s fields, which `RunParams` carries beside its own.
fn run_task_fields() -> Vec<Field> {
    let text = || Ty::optional(Ty::Str);
    let path = || Ty::optional(Ty::Path);
    let none = || Value::Null;
    let empty = || Value::Array(vec![]);
    let count = |maximum: u32| Ty::Int.constrained("minimum", 0).constrained("maximum", maximum);
    vec![
        Field::required("task", Ty::Str),
        Field::default("start", text(), none()),
        Field::default("context", text(), none()),
        Field::default("inputs", Ty::optional(Ty::map(Ty::Str)), none()),
        Field::default("attachments", Ty::list(RunAttachment::ty()), empty()),
        Field::default("limits", Ty::optional(Limits::ty()), none()),
        Field::default("authorization", Ty::optional(Authorization::ty()), none()),
        Field::default("output_schema", Ty::optional(Ty::map(Ty::Any)), none()),
        Field::default("downloads", path(), none()),
        Field::default("record", path(), none()),
        Field::default("cursor", Ty::Bool, false),
        Field::default("local", Ty::Bool, false),
        Field::default("chrome", Ty::optional(LocalChrome::ty()), none()),
        Field::default("cloud_profile", text(), none()),
        Field::default("cdp_url", text(), none()),
        Field::default("cdp_port", Ty::optional(count(u16::MAX.into())), none()),
        Field::default("attach", Ty::Bool, false),
        Field::default("target_match", text(), none()),
        Field::default("foreground", Ty::Bool, false),
        Field::default("proxy_country", text(), none()),
        Field::default("viewport", Ty::optional(Ty::Tuple(vec![count(u32::MAX), count(u32::MAX)])), none()),
        Field::default("cloud_extensions", Ty::list(Ty::Str), empty()),
        Field::default("cloud_timeout_minutes", Ty::optional(count(u32::MAX)), none()),
        Field::default("allowed_origins", Ty::optional(Ty::list(Ty::Str)), none()),
        Field::default("fetch", Ty::Bool, false),
        Field::default("fetch_first", Ty::Bool, false),
    ]
}

impl Schema for RunParams {
    fn ty() -> Ty {
        let mut fields = vec![
            Field::required("run_id", Ty::Str.constrained("minLength", 1)),
            Field::default("secrets", Ty::list(SecretRef::ty()), Value::Array(vec![])),
            Field::default("frames", Ty::Bool, false),
            Field::default("until", Ty::Bool, false),
        ];
        fields.extend(run_task_fields());
        params(
            "RunParams",
            Some(
                "One run: the run's arguments, and what the server needs to talk to the client about it.\n\nThe reply is the `RunResult`. No API key is among the fields: model and browser keys come from the\nenvironment the server inherited.",
            ),
            fields,
        )
        .ty()
    }
}

impl Schema for RunCancelParams {
    fn ty() -> Ty {
        params(
            "RunCancelParams",
            Some(
                "Which run to stop. One that is not active, because it finished or never was, is left as it is.\n\nThe reply says only that the request was read. The run's own reply is what says it was cancelled: the\n`cancelled` error, written after its browser has closed.",
            ),
            vec![Field::required("run_id", Ty::Str)],
        )
        .ty()
    }
}

impl Schema for RunEvent {
    fn ty() -> Ty {
        message(
            "RunEvent",
            Some("The params of `run/event`: one thing a run did, written before that run's reply."),
            vec![Field::required("run_id", Ty::Str), Field::required("event", Event::ty())],
        )
        .ty()
    }
}

impl Schema for RunFrame {
    fn ty() -> Ty {
        message(
            "RunFrame",
            Some(
                "The params of `run/frame`: the active tab as it looked a moment ago. Sent only to a run that set `frames`.",
            ),
            vec![Field::required("run_id", Ty::Str), Field::required("frame", Ty::Image)],
        )
        .ty()
    }
}

impl Schema for Notification {
    fn ty() -> Ty {
        message(
            "Notification",
            Some(
                "A message from the server that takes no reply. It has no `id` member at all, which is what marks it.",
            ),
            vec![jsonrpc(), Field::required("method", ServerMethod::ty()), Field::required("params", Ty::Any)],
        )
        .ty()
    }
}

impl Schema for ServerRequest {
    fn ty() -> Ty {
        message(
            "ServerRequest",
            Some(
                "A call from the server that the client answers, with a `Response` or an `ErrorResponse` carrying its id.\n\nThe ids are the server's own and count up from one. A client's ids may be the same numbers: a reply is\ntold from a request by having no `method`, so the two never meet.",
            ),
            vec![
                jsonrpc(),
                Field::required("id", Ty::Int),
                Field::required("method", ServerMethod::ty()),
                Field::required("params", Ty::Any),
            ],
        )
        .ty()
    }
}

impl Schema for SecretsResolveParams {
    fn ty() -> Ty {
        message(
            "SecretsResolveParams",
            Some(
                "The params of `secrets/resolve`: one value the run is about to type. The reply is the value, or null.",
            ),
            vec![
                Field::required("run_id", Ty::Str),
                Field::required("name", Ty::Str),
                Field::required("origin", Ty::Str),
            ],
        )
        .ty()
    }
}

impl Schema for RunUntilParams {
    fn ty() -> Ty {
        message(
            "RunUntilParams",
            Some(
                "The params of `run/until`: the address a run ended on. The reply is a boolean, and false keeps the run\nfrom `complete`.",
            ),
            vec![Field::required("run_id", Ty::Str), Field::required("url", Ty::Str)],
        )
        .ty()
    }
}

/// `SCREAMING_SNAKE` of a variant's Rust name, the name the member had in Python.
fn member_name(variant: impl std::fmt::Debug) -> String {
    let mut name = String::new();
    for (index, letter) in format!("{variant:?}").chars().enumerate() {
        if letter.is_uppercase() && index > 0 {
            name.push('_');
        }
        name.push(letter.to_ascii_uppercase());
    }
    name
}

/// Every message on the wire as JSON Schema, one definition each under `$defs`, and beside them under `constants`
/// the values a client has to hold the same as the server: the protocol version, the error codes and the method
/// names. `constants` is no part of JSON Schema. Each client's types are generated from this document, which is
/// committed as `protocol/schema.json`.
///
/// Params are what the server reads, so a field with a default may be left out of them. Everything else is what it
/// writes, where every field is there.
pub fn schema() -> Value {
    let read = |ty: Ty| (ty, Mode::Validation);
    let written = |ty: Ty| (ty, Mode::Serialization);
    let mut document = definitions(&[
        read(params(
            "Params",
            Some("A method's params. An unknown field is refused, so a misspelled option is never dropped in silence."),
            vec![],
        )
        .ty()),
        read(NoParams::ty()),
        read(RunAttachment::ty()),
        read(RunParams::ty()),
        read(RunCancelParams::ty()),
        written(Request::ty()),
        written(Response::ty()),
        written(Error::ty()),
        written(ErrorResponse::ty()),
        written(InitializeParams::ty()),
        written(InitializeResult::ty()),
        written(RunEvent::ty()),
        written(RunFrame::ty()),
        written(Notification::ty()),
        written(ServerRequest::ty()),
        written(SecretsResolveParams::ty()),
        written(RunUntilParams::ty()),
        written(RunResult::ty()),
    ]);
    let members = |pairs: Vec<(String, Value)>| Value::Object(pairs.into_iter().collect());
    document["constants"] = serde_json::json!({
        "ErrorCode": members(ErrorCode::ALL.iter().map(|code| (member_name(code), code.code().into())).collect()),
        "Method": members(Method::ALL.iter().map(|method| (member_name(method), method.as_str().into())).collect()),
        "PROTOCOL_VERSION": PROTOCOL_VERSION,
        "ServerMethod": members(ServerMethod::ALL.iter().map(|method| (member_name(method), method.as_str().into())).collect()),
    });
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::py::dump::Dump;
    use serde_json::json;

    #[test]
    fn a_request_id_is_an_int_or_a_string_and_nothing_else() {
        assert!(serde_json::from_value::<Request>(json!({"id": true, "method": "x"})).is_err());
        assert!(serde_json::from_value::<Request>(json!({"id": 1.0, "method": "x"})).is_err());
        let request: Request = serde_json::from_value(json!({"id": "a", "method": "x", "params": [1]})).unwrap();
        assert_eq!(request.dump_json(), r#"{"jsonrpc":"2.0","id":"a","method":"x","params":[1]}"#);
        let notification: Request = serde_json::from_value(json!({"method": "x", "extra": 1})).unwrap();
        assert_eq!(notification.dump_json(), r#"{"jsonrpc":"2.0","id":null,"method":"x","params":null}"#);
    }

    #[test]
    fn an_error_reply_writes_its_code_as_a_number() {
        let reply = ErrorResponse {
            jsonrpc: JsonRpc,
            id: None,
            error: Error { code: ErrorCode::ParseError, message: "bad".into() },
        };
        assert_eq!(reply.dump_json(), r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"bad"}}"#);
    }

    #[test]
    fn run_params_refuse_what_they_do_not_name() {
        let unknown = serde_json::from_value::<RunParams>(json!({"run_id": "r", "task": "t", "tsak": 1}));
        assert!(unknown.unwrap_err().to_string().contains("unknown field `tsak`"));
        assert!(serde_json::from_value::<RunParams>(json!({"run_id": "", "task": "t"})).is_err());
        assert!(serde_json::from_value::<RunParams>(json!({"run_id": "r", "task": "t", "cdp_port": 70000})).is_err());
        let params: RunParams = serde_json::from_value(json!({"run_id": "r", "task": "t", "frames": true, "attachments": [{"name": "a", "mime_type": "text/plain", "content": "aGVs bG8="}], "viewport": [1280, 720]}))
            .unwrap();
        assert!(params.frames);
        assert_eq!(params.run.attachments[0].content.0, b"hello");
        assert_eq!(params.run.viewport, Some((1280, 720)));
        let written = serde_json::to_value(&params).unwrap();
        assert_eq!(serde_json::from_value::<RunParams>(written).unwrap(), params);
        let refused =
            serde_json::from_value::<RunAttachment>(json!({"name": "a", "mime_type": "m", "content": "aGVsbG8"}));
        assert!(refused.unwrap_err().to_string().contains("Base64 decoding error: 'Incorrect padding'"));
    }
}
