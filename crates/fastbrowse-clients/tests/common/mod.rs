//! What the client tests share: a scripted transport, as Python's tests use `httpx.MockTransport`, and response models
//! that validate as pydantic does for the cases the tests reach.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use fastbrowse_clients::http::{Http, Request, Response, Transport, TransportError};
use fastbrowse_clients::validation::{errors_json, line_error};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::ResponseSchema;
use fastbrowse_core::py::lax::{bool_of, int_of};
use fastbrowse_core::schema::{Field, ModelTy, Ty, json_schema};
use fastbrowse_core::telemetry::Trace;
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

/// One scripted reply.
#[derive(Debug, Clone)]
pub enum Step {
    Reply(Response),
    Fail(TransportError),
    /// Never answers; dropped when the call no longer wants it.
    Stall,
    /// Signals the test to cancel the call, then never answers.
    Cancel,
}

pub fn reply(status: u16, body: Value) -> Step {
    Step::Reply(Response::json(status, &body))
}

pub fn text(status: u16, body: &str) -> Step {
    Step::Reply(Response::new(status, body))
}

#[derive(Default)]
struct State {
    steps: VecDeque<Step>,
    requests: Vec<Request>,
}

/// Hands back scripted replies in the order requests arrive, and keeps every request.
#[derive(Clone, Default)]
pub struct Script {
    state: Arc<Mutex<State>>,
    pub cancel: Arc<Notify>,
}

impl Script {
    pub fn new(steps: impl IntoIterator<Item = Step>) -> Script {
        let script = Script::default();
        script.state.lock().unwrap().steps.extend(steps);
        script
    }

    pub fn http(&self) -> Http {
        Http::new(Arc::new(self.clone()))
    }

    pub fn traced(&self, trace: &Trace) -> Http {
        self.http().with_trace(trace.clone())
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn bodies(&self) -> Vec<Value> {
        self.requests().iter().map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null)).collect()
    }

    pub fn unused(&self) -> usize {
        self.state.lock().unwrap().steps.len()
    }
}

impl Transport for Script {
    fn send(&self, request: Request) -> BoxFuture<'static, Result<Response, TransportError>> {
        let step = {
            let mut state = self.state.lock().unwrap();
            state.requests.push(request.clone());
            state.steps.pop_front()
        };
        let cancel = self.cancel.clone();
        Box::pin(async move {
            match step {
                Some(Step::Reply(response)) => Ok(response),
                Some(Step::Fail(error)) => Err(error),
                Some(Step::Stall) => {
                    tokio::time::sleep(request.timeout).await;
                    Err(TransportError::Timeout("TimeoutError"))
                }
                Some(Step::Cancel) => {
                    cancel.notify_one();
                    std::future::pending().await
                }
                None => panic!("no scripted response for {}", request.url),
            }
        })
    }
}

/// A transport answering every request with what `answer` returns for it.
pub fn answering<F>(answer: F) -> Http
where
    F: Fn(&Request) -> BoxFuture<'static, Result<Response, TransportError>> + Send + Sync + 'static,
{
    Http::new(Arc::new(move |request: Request| answer(&request)))
}

/// A response model whose schema is pydantic's for `ty` and whose validation reports pydantic's errors.
pub struct Model(pub Ty);

impl Model {
    fn model(&self) -> &ModelTy {
        match &self.0 {
            Ty::Model(model) => model,
            _ => panic!("not a model"),
        }
    }
}

impl ResponseSchema for Model {
    fn name(&self) -> String {
        self.model().name.clone()
    }

    fn json_schema(&self) -> Value {
        json_schema(&self.0)
    }

    fn validate_json(&self, text: &str) -> Result<Value, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut errors = Vec::new();
        let dumped = validate_model(self.model(), &value, &[], &mut errors);
        if errors.is_empty() { Ok(dumped) } else { Err(errors_json(errors)) }
    }
}

fn at(loc: &[Value], key: impl Into<Value>) -> Vec<Value> {
    let mut loc = loc.to_vec();
    loc.push(key.into());
    loc
}

fn validate_model(model: &ModelTy, value: &Value, loc: &[Value], errors: &mut Vec<Value>) -> Value {
    let Some(object) = value.as_object() else {
        errors.push(line_error(
            "model_type",
            loc.to_vec(),
            "Input should be an object",
            Some(json!({"class_name": model.name})),
        ));
        return Value::Null;
    };
    let mut dumped = Map::new();
    for field in &model.fields {
        match (object.get(field.key()), &field.default) {
            (Some(item), _) => {
                dumped.insert(field.name.clone(), validate(&field.ty, item, &at(loc, field.key()), errors));
            }
            (None, Some(default)) => {
                dumped.insert(field.name.clone(), default.clone());
            }
            (None, None) => errors.push(line_error("missing", at(loc, field.key()), "Field required", None)),
        }
    }
    if model.forbid_extra {
        for key in object.keys().filter(|key| !model.fields.iter().any(|f| f.key() == key.as_str())) {
            errors.push(line_error("extra_forbidden", at(loc, key.as_str()), "Extra inputs are not permitted", None));
        }
    }
    Value::Object(dumped)
}

fn validate(ty: &Ty, value: &Value, loc: &[Value], errors: &mut Vec<Value>) -> Value {
    let mut fail = |kind: &str, msg: &str| {
        errors.push(line_error(kind, loc.to_vec(), msg, None));
        Value::Null
    };
    match ty {
        Ty::Str => match value {
            Value::String(_) => value.clone(),
            _ => fail("string_type", "Input should be a valid string"),
        },
        Ty::Int => match value {
            Value::Number(n) if n.is_i64() => value.clone(),
            Value::Number(n) if n.as_f64().is_some_and(|f| f.fract() != 0.0) => {
                fail("int_from_float", "Input should be a valid integer, got a number with a fractional part")
            }
            Value::String(_) => match int_of(value) {
                Ok(i) => i.into(),
                Err(_) => fail("int_parsing", "Input should be a valid integer, unable to parse string as an integer"),
            },
            _ => match int_of(value) {
                Ok(i) => i.into(),
                Err(_) => fail("int_type", "Input should be a valid integer"),
            },
        },
        Ty::Bool => match bool_of(value) {
            Ok(b) => b.into(),
            Err(_) => fail("bool_parsing", "Input should be a valid boolean"),
        },
        Ty::List(item) => match value {
            Value::Array(items) => {
                items.iter().enumerate().map(|(i, v)| validate(item, v, &at(loc, i), errors)).collect()
            }
            _ => fail("tuple_type", "Input should be a valid tuple"),
        },
        Ty::Optional(inner) => {
            if value.is_null() {
                Value::Null
            } else {
                validate(inner, value, loc, errors)
            }
        }
        Ty::Literal(values) => {
            if values.contains(value) {
                value.clone()
            } else {
                let names: Vec<String> =
                    values.iter().map(|v| format!("'{}'", v.as_str().unwrap_or_default())).collect();
                let expected = match names.split_last() {
                    Some((last, [])) => last.clone(),
                    Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                    None => String::new(),
                };
                errors.push(line_error(
                    "literal_error",
                    loc.to_vec(),
                    &format!("Input should be {expected}"),
                    Some(json!({"expected": expected})),
                ));
                Value::Null
            }
        }
        Ty::Model(model) => validate_model(model, value, loc, errors),
        Ty::Constrained(inner, _) => validate(inner, value, loc, errors),
        _ => value.clone(),
    }
}

/// `Result`: a count and what was counted.
pub fn result_model() -> Model {
    Model(
        ModelTy::frozen(
            "Result",
            vec![
                Field::required("count", Ty::Int),
                Field::default("tags", Ty::list(Ty::Str), json!([])).doc("What was counted."),
            ],
        )
        .ty(),
    )
}

pub fn answer_model() -> Model {
    Model(ModelTy::frozen("Answer", vec![Field::required("answer", Ty::Str)]).ty())
}

pub fn plan_model() -> Model {
    let requirement = ModelTy {
        module: "__main__",
        ..ModelTy::frozen(
            "Requirement",
            vec![Field::required("text", Ty::Str), Field::required("kind", Ty::literals(&["fact", "action"]))],
        )
    }
    .ty();
    Model(
        ModelTy {
            module: "__main__",
            ..ModelTy::frozen(
                "Plan",
                vec![
                    Field::required("requirements", Ty::list(requirement)),
                    Field::required("answer_expected", Ty::Bool),
                    Field::default("note", Ty::optional(Ty::Str), Value::Null),
                ],
            )
            .doc("What the run must show before it may finish.")
        }
        .ty(),
    )
}
