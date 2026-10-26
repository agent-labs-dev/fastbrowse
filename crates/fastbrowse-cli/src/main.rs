//! `fastbrowse`: the CLI, `fastbrowse serve --stdio`, and, run under the name `fastbrowse-mcp`, the MCP server.

use std::sync::Arc;

use fastbrowse_cli::argparse::{Exit, Opt, Parser};
use fastbrowse_cli::cli::{self, Context, Streams};
use fastbrowse_cli::runner::default_runner;
use fastbrowse_cli::serve::{self, Server, StdioTransport, Transport};
use fastbrowse_cli::{log, mcp, options};

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
        if flag == "--run-task" && fastbrowse_cli::scripted::named(value, Arc::default()).is_none() {
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
    let (runner, protocol, transport) = fastbrowse_cli::scripted::setup(parsed.value("--run-task"), transport);
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
        fastbrowse_cli::scripted::linger().await;
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
        .and_then(|name| fastbrowse_cli::scripted::named(name, Arc::default()))
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

fn main() {
    let argv: Vec<String> = std::env::args_os().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let program = argv.first().map(std::path::Path::new).and_then(|path| path.file_stem());
    let code = if program.is_some_and(|name| name == "fastbrowse-mcp") {
        mcp_command(&argv[1..])
    } else if argv.get(1).is_some_and(|arg| arg == "serve") {
        // Only as the first argument, so a task that mentions the word, or `--local serve`, is still a task.
        serve_command(&argv[2..])
    } else {
        // Retries, failovers and a recording that could not be written are warnings; say whose they are.
        log::init("fastbrowse");
        let context = Context {
            runner: default_runner(),
            settings: serve::load_settings(),
            environ: options::environ(),
            streams: Streams::stdio(),
        };
        runtime().block_on(cli::main(&argv[1..], &context))
    };
    std::process::exit(code);
}
