//! `run_task` for a host language in this process: the Python module and the Node addon call it.
//!
//! A run's params and its result are the JSON `serve` reads and writes, and its callbacks are the host's own
//! functions, so a binding converts values and adds no rule of its own. A request `serve` refuses is refused here
//! with the same code and message, and a callback that fails ends the run as the client's failure does there.

use std::sync::{Arc, Mutex};

use fastbrowse_clients::environment::Settings;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{Event, RunResult};
use fastbrowse_core::protocol::{ErrorCode, Method, RunParams, ServerMethod};
use fastbrowse_core::py::dump::Dump;
use serde_json::Value;

use crate::runner::{Cancel, Runner, default_runner};
use crate::serve::{Callbacks, ClientError, Refused, SettingsSource, arguments, read, reply, secret_failure};

/// A host function the run awaits. An `Err` carries the message of what the host raised or threw.
pub type HostCall<A, R> = Arc<dyn Fn(A) -> BoxFuture<'static, Result<R, String>> + Send + Sync>;

/// `(name, origin)` to the value the host holds, or None.
pub type HostResolve = Arc<dyn Fn(String, String) -> BoxFuture<'static, Result<Option<String>, String>> + Send + Sync>;

/// The host's side of a run's callbacks. Each stands for the `serve` message of the same purpose.
#[derive(Clone, Default)]
pub struct Host {
    /// `run/event`: each event as the JSON that message carries in `event`.
    pub on_event: Option<HostCall<Value, ()>>,
    /// `run/frame`: one JPEG. Needed when the params set `frames`.
    pub on_frame: Option<HostCall<Vec<u8>, ()>>,
    /// `run/until`. Needed when the params set `until`.
    pub until: Option<HostCall<String, bool>>,
    /// `secrets/resolve`, asked only for an origin the ref covers. Needed when the params name secrets. What it fails
    /// with is never repeated, since it may quote the value.
    pub resolve_secret: Option<HostResolve>,
}

/// Runs for one host: the runner, and the settings each run is checked against.
pub struct Embedded {
    runner: Arc<dyn Runner>,
    settings: SettingsSource,
    /// The params of every run, as the stand-in runs of the SDK's tests read them.
    #[cfg(feature = "scripted")]
    requests: Option<crate::scripted::Requests>,
}

impl Embedded {
    /// Runs with the real `run_task`. Settings are read for each run from the process environment with `env` over
    /// it, then `.env` in the working directory, as `serve` reads its own.
    ///
    /// With the `scripted` feature, FASTBROWSE_TEST_SERVER=scripted_run in `env` puts the stand-in FASTBROWSE_TEST_RUN
    /// names in place of `run_task`, as it does for the binary.
    pub fn new(env: Vec<(String, String)>) -> Embedded {
        #[cfg(feature = "scripted")]
        let (runner, requests) = {
            let named = |name: &str| env.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.clone());
            let requests = crate::scripted::Requests::default();
            match named("FASTBROWSE_TEST_SERVER").as_deref() {
                Some("scripted_run") => {
                    let run = format!("scripted_run:{}", named("FASTBROWSE_TEST_RUN").unwrap_or_default());
                    (Some(crate::scripted::known(&run, requests.clone())), Some(requests))
                }
                _ => (None, None),
            }
        };
        #[cfg(not(feature = "scripted"))]
        let runner: Option<Arc<dyn Runner>> = None;
        let settings: SettingsSource = Arc::new(move || {
            let dotenv = std::fs::read_to_string(".env").ok();
            let mut vars: Vec<(String, String)> = std::env::vars().collect();
            vars.extend(env.iter().cloned());
            Settings::from_sources(&vars, dotenv.as_deref())
        });
        Embedded {
            runner: runner.unwrap_or_else(default_runner),
            settings,
            #[cfg(feature = "scripted")]
            requests,
        }
    }

    /// Runs with `runner` in place of `run_task`, for a test.
    pub fn with_runner(runner: Arc<dyn Runner>, settings: SettingsSource) -> Embedded {
        Embedded {
            runner,
            settings,
            #[cfg(feature = "scripted")]
            requests: None,
        }
    }

    /// Run the task `params` describes, a `run` request's params as JSON, until it ends or `cancel` is tripped.
    pub async fn run(&self, params: &str, host: Host, cancel: Cancel) -> Result<RunResult, Refused> {
        let params: Value =
            serde_json::from_str(params).map_err(|error| refusal(format!("run: params are not JSON: {error}")))?;
        #[cfg(feature = "scripted")]
        if let Some(requests) = &self.requests {
            requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(params.clone());
        }
        let params: RunParams = read(Method::Run, params)?;
        // `serve` asks the client for what the params say it holds, so a host that holds less has to say so here.
        if params.frames && host.on_frame.is_none() {
            return Err(refusal("run: frames is set and no frame callback was given".into()));
        }
        if params.until && host.until.is_none() {
            return Err(refusal("run: until is set and no until callback was given".into()));
        }
        if !params.secrets.is_empty() && host.resolve_secret.is_none() {
            return Err(refusal("run: secrets are named and no resolver was given".into()));
        }
        let failed = Failed { cancel: cancel.clone(), failure: Arc::default() };
        let callbacks = callbacks(host, &failed);
        let run_id = params.run_id.clone();
        let (task, run_host) = arguments(params, &self.settings, callbacks)?;
        let outcome = self.runner.run(task, run_host).await;
        let failure = failed.failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        reply(outcome, failure, &run_id)
    }
}

fn refusal(message: String) -> Refused {
    Refused { code: ErrorCode::InvalidParams, message }
}

/// The first callback the host failed, which stops the run.
#[derive(Clone)]
struct Failed {
    cancel: Cancel,
    failure: Arc<Mutex<Option<ClientError>>>,
}

impl Failed {
    fn fail<T: Default>(&self, method: ServerMethod, message: String) -> T {
        let failure = ClientError { method, message, code: None };
        self.failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert(failure);
        self.cancel.trip();
        T::default()
    }
}

fn callbacks(host: Host, failed: &Failed) -> Callbacks {
    let Host { on_event, on_frame, until, resolve_secret } = host;
    let events = failed.clone();
    let frames = failed.clone();
    let checks = failed.clone();
    let secrets = failed.clone();
    Callbacks {
        cancel: failed.cancel.clone(),
        on_event: Arc::new(move |event: Event| {
            let (on_event, failed) = (on_event.clone(), events.clone());
            Box::pin(async move {
                if let Some(on_event) = on_event
                    && let Err(message) = on_event(event.dump()).await
                {
                    failed.fail::<()>(ServerMethod::RunEvent, message);
                }
            })
        }),
        on_frame: on_frame.map(|on_frame| -> fastbrowse_core::models::FrameHandler {
            Arc::new(move |frame: Vec<u8>| {
                let (on_frame, failed) = (on_frame.clone(), frames.clone());
                Box::pin(async move {
                    if let Err(message) = on_frame(frame).await {
                        failed.fail::<()>(ServerMethod::RunFrame, message);
                    }
                })
            })
        }),
        until: until.map(|until| -> fastbrowse_core::models::UntilCheck {
            Arc::new(move |url: String| {
                let (until, failed) = (until.clone(), checks.clone());
                Box::pin(async move {
                    until(url).await.unwrap_or_else(|message| failed.fail(ServerMethod::RunUntil, message))
                })
            })
        }),
        resolve: resolve_secret.map(|resolve| -> crate::serve::Resolve {
            Arc::new(move |name: String, origin: String| {
                let (resolve, failed) = (resolve.clone(), secrets.clone());
                Box::pin(async move {
                    let failure = secret_failure(&name);
                    resolve(name, origin).await.unwrap_or_else(|_| failed.fail(ServerMethod::SecretsResolve, failure))
                })
            })
        }),
    }
}
