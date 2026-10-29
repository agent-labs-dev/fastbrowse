//! `fastbrowse`: the CLI, `fastbrowse serve --stdio`, and, run under the name `fastbrowse-mcp`, the MCP server.

fn main() {
    let argv: Vec<String> = std::env::args_os().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let program = argv.first().cloned().unwrap_or_default();
    std::process::exit(fastbrowse_cli::entry::main(&program, argv.get(1..).unwrap_or_default(), Default::default()));
}
