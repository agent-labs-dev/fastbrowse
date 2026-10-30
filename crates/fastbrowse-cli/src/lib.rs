//! The `fastbrowse` binary's entry points, ported from `src/fastbrowse`: the CLI (`cli.py`), `fastbrowse serve --stdio`
//! (`serve.py`), the MCP server (`mcp_server.py`, run as `fastbrowse-mcp`) and the rules they share (`options.py`).
//!
//! Each ends in a call to `run_task`, which [`runner::Runner`] stands for. The modules are a library so the tests drive
//! them in process, as Python's tests drive the server over in-memory streams.

pub mod argparse;
pub mod bitwarden;
pub mod cli;
pub mod embed;
pub mod entry;
pub mod mcp;
pub mod options;
pub mod runner;
#[cfg(feature = "scripted")]
pub mod scripted;
pub mod serve;
pub mod totp;

/// `logging` as the entry points configure it: warnings and worse on stderr, named by whose they are.
pub mod log {
    static PREFIX: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

    struct Stderr;

    impl ::log::Log for Stderr {
        fn enabled(&self, metadata: &::log::Metadata<'_>) -> bool {
            metadata.level() <= ::log::Level::Warn
        }

        fn log(&self, record: &::log::Record<'_>) {
            if self.enabled(record.metadata()) {
                let level = match record.level() {
                    ::log::Level::Error => "ERROR",
                    _ => "WARNING",
                };
                eprintln!("{}: {level} {}", PREFIX.get().copied().unwrap_or("fastbrowse"), record.args());
            }
        }

        fn flush(&self) {}
    }

    /// `logging.basicConfig(stream=sys.stderr, level=logging.WARNING, format="{prefix}: %(levelname)s %(message)s")`.
    pub fn init(prefix: &'static str) {
        let _ = PREFIX.set(prefix);
        if ::log::set_logger(&Stderr).is_ok() {
            ::log::set_max_level(::log::LevelFilter::Warn);
        }
    }

    pub fn warning(message: &str) {
        ::log::warn!("{message}");
    }

    pub fn error(message: &str) {
        ::log::error!("{message}");
    }
}
