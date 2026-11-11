//! The `fastbrowse` command: the CLI, `fastbrowse serve --stdio`, and, run under the name `fastbrowse-mcp`, the MCP
//! server. The binary calls [`main`], and so does the Python package's `fastbrowse` command, through its module.

use std::sync::Arc;

use crate::argparse::{Exit, Opt, Parser};
use crate::cli::{self, Build, Context, Streams};
use crate::runner::default_runner;
use crate::serve::{self, Server, StdioTransport, Transport};
use crate::{log, mcp, options};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime starts")
}

fn report(exit: Exit) -> i32 {
    match exit {
        Exit::Print(text) => {
            print!("{text}");
            0
        }
        Exit::Usage(text) => {
            eprint!("{text}");
            2
        }
    }
}

/// `trace_to()`: the trace goes where `FASTBROWSE_TRACE` says, if anywhere.
fn trace_from_environment(name: &str) -> Result<(), i32> {
    fastbrowse_core::telemetry::trace_to(None).map_err(|error| {
        eprintln!("{name}: cannot write the trace: {error}");
        1
    })
}

fn serve_command(argv: &[String]) -> i32 {
    let mut options = vec![Opt::flag("--stdio").required()];
    // Tests that start this command need a run with no model behind it. It is left out of the help because it is no
    // part of what the command offers.
    if cfg!(feature = "scripted") {
        options.push(Opt::value("--run-task"));
    }
    let parser = Parser {
        prog: "fastbrowse serve",
        help: include_str!("../generated/serve.txt"),
        version: None,
        options,
        positional: None,
        exclusive: Vec::new(),
    };
    let check = |flag: &str, value: &str| {
        #[cfg(feature = "scripted")]
        if flag == "--run-task" && crate::scripted::named(value, Arc::default()).is_none() {
            let name = fastbrowse_core::py::text::repr(value);
            return Err(format!("{name} names nothing to call: no stand-in run of that name"));
        }
        let _ = (flag, value);
        Ok(())
    };
    let parsed = match parser.parse(argv, check) {
        Ok(parsed) => parsed,
        Err(exit) => return report(exit),
    };
    let transport: Arc<dyn Transport> = Arc::new(StdioTransport::start());
    #[cfg(feature = "scripted")]
    let (runner, protocol, transport) = crate::scripted::setup(parsed.value("--run-task"), transport);
    #[cfg(not(feature = "scripted"))]
    let (runner, protocol) = {
        let _ = parsed;
        (default_runner(), fastbrowse_core::protocol::PROTOCOL_VERSION)
    };
    log::init("fastbrowse");
    if let Err(code) = trace_from_environment("fastbrowse") {
        return code;
    }
    runtime().block_on(async move {
        let code = Server::with_protocol(transport, runner, serve::load_settings(), protocol).serve().await;
        #[cfg(feature = "scripted")]
        crate::scripted::linger().await;
        code
    })
}

fn mcp_command(argv: &[String]) -> i32 {
    // stdout carries the protocol under stdio, so every log line goes to stderr.
    log::init("fastbrowse-mcp");
    if let Err(code) = trace_from_environment("fastbrowse-mcp") {
        return code;
    }
    let args = match mcp::parse(argv) {
        Ok(args) => args,
        Err(exit) => return report(exit),
    };
    let configured = fastbrowse_clients::environment::Settings::load()
        .and_then(|settings| mcp::configure(&args, &settings, &options::environ()));
    let config = match configured {
        Ok(config) => config,
        Err(error) => {
            eprintln!("fastbrowse-mcp: {}", error.0);
            return 1;
        }
    };
    #[cfg(feature = "scripted")]
    let runner = args
        .run_task
        .as_deref()
        .and_then(|name| crate::scripted::named(name, Arc::default()))
        .unwrap_or_else(default_runner);
    #[cfg(not(feature = "scripted"))]
    let runner = default_runner();
    let server = mcp::McpServer::new(config, runner);
    runtime().block_on(async move {
        if args.transport == "stdio" {
            mcp::serve_stdio(server).await;
            return 0;
        }
        match mcp::http::serve(server, &args.host, args.port).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("fastbrowse-mcp: {error}");
                1
            }
        }
    })
}

/// Run the command `program` names with `args`, the arguments after the program, and return its exit code. `build`
/// is the commit the CLI reports.
pub fn main(program: &str, args: &[String], build: Build) -> i32 {
    // Before any runtime, logger or argument parsing: a helper writes nothing but its reply.
    if let Some(code) = fastbrowse_run::helper::serve(args) {
        return code;
    }
    let program_name = program;
    let program = std::path::Path::new(program).file_stem();
    if program.is_some_and(|name| name == "fastbrowse-mcp") {
        mcp_command(args)
    } else if args.first().is_some_and(|arg| arg == "native-host") {
        // Chrome starts this when the extension connects, with the extension's origin as the first argument.
        let origin = args.get(1).map_or("", String::as_str);
        match runtime().block_on(fastbrowse_browser::extension::host::run(origin)) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("fastbrowse native-host: {error}");
                1
            }
        }
    } else if args.first().is_some_and(|arg| arg == "extension") {
        let (text, code) = crate::extension::command(program_name, &args[1..]);
        if code == 0 {
            print!("{text}");
        } else {
            eprint!("{text}");
        }
        code
    } else if args.first().is_some_and(|arg| arg == "self-update") {
        let (text, code) = crate::update::command(&args[1..], &build);
        if code == 0 {
            print!("{text}");
        } else {
            eprint!("{text}");
        }
        code
    } else if args.first().is_some_and(|arg| arg == "serve") {
        // Only as the first argument, so a task that mentions the word, or `--local serve`, is still a task.
        serve_command(&args[1..])
    } else {
        // Retries, failovers and a recording that could not be written are warnings; say whose they are.
        log::init("fastbrowse");
        let context = Context {
            runner: default_runner(),
            settings: serve::load_settings(),
            environ: options::environ(),
            streams: Streams::stdio(),
            build,
        };
        runtime().block_on(until_interrupted(cli::main(args, &context), interrupted(), INTERRUPT_SETTLE))
    }
}

/// How long an interrupted run gets to stop what it started, such as a cloud browser that bills until its timeout.
const INTERRUPT_SETTLE: std::time::Duration = std::time::Duration::from_secs(15);

/// `run`'s exit code, or `interrupt`'s when that ends first. An interrupted run is dropped, which sets off the stops
/// its resources' `Drop` spawns, and those get up to `settle` to finish before the process exits (#773).
async fn until_interrupted(
    run: impl std::future::Future<Output = i32>,
    interrupt: impl std::future::Future<Output = i32>,
    settle: std::time::Duration,
) -> i32 {
    let mut run = Box::pin(run);
    let code = tokio::select! {
        code = &mut run => return code,
        code = interrupt => code,
    };
    drop(run);
    eprintln!("fastbrowse: interrupted, stopping the browser");
    let metrics = tokio::runtime::Handle::current().metrics();
    let _ = tokio::time::timeout(settle, async {
        while metrics.num_alive_tasks() > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    code
}

/// The exit code of the first Ctrl-C or SIGTERM: 130 or 143, as a shell reports a process those signals ended.
async fn interrupted() -> i32 {
    let ctrl_c = async {
        // A handler that cannot be installed leaves the signal to end the process as it always did.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut terminate) => {
                    terminate.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = ctrl_c => 130,
            () = terminate => 143,
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await;
        130
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::until_interrupted;

    /// Spawns, when dropped, a task that marks `stopped` a little later, as a cloud browser's stop does.
    struct StopsOnDrop(Arc<AtomicBool>);

    impl Drop for StopsOnDrop {
        fn drop(&mut self) {
            let stopped = self.0.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                stopped.store(true, Ordering::SeqCst);
            });
        }
    }

    #[tokio::test]
    async fn an_interrupted_run_stops_what_it_started_before_exiting() {
        let stopped = Arc::new(AtomicBool::new(false));
        let guard = StopsOnDrop(stopped.clone());
        let run = async move {
            let _guard = guard;
            std::future::pending::<i32>().await
        };
        let code = until_interrupted(run, std::future::ready(130), Duration::from_secs(5)).await;
        assert_eq!(code, 130);
        assert!(stopped.load(Ordering::SeqCst), "the stop the run's drop spawned was cut off");
    }

    #[tokio::test]
    async fn a_run_that_ends_first_gives_its_own_code() {
        assert_eq!(until_interrupted(async { 3 }, std::future::pending(), Duration::ZERO).await, 3);
    }
}
