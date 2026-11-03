# fastbrowse

A browser agent that picks instead of generating. Every page is indexed into the controls it actually has, and a
choice model picks one of them. An LLM plans, reads and writes prose. The choice model also decides when a page is
read and when the run is done. Code owns authorization gates, secret resolution and cost limits.

```toml
[dependencies]
fastbrowse = "0.9"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use fastbrowse::{Host, Limits, RunTask, run_task};

#[tokio::main]
async fn main() -> Result<(), fastbrowse::RunError> {
    // A run reads a PDF's text in this program run again as a helper process, which it can kill when time is up.
    fastbrowse::helper::run_if_requested();
    let task = RunTask {
        task: "What is the top story's title?".into(),
        start: Some("https://news.ycombinator.com/".into()),
        limits: Some(Limits { max_dollars: Some(0.25), ..Limits::default() }),
        ..RunTask::default()
    };
    let result = run_task(task, Host::default()).await?;
    println!("{:?}: {}", result.status, result.answer.unwrap_or_default());
    Ok(())
}
```

Model keys come from the environment: `TYPESAFE_API_KEY` for Jev and `OPENROUTER_API_KEY` for the LLM. The run
starts local Chrome, or a Browser Use Cloud browser when `Host::browser_api_key` is given. The [project README](https://github.com/agent-labs-dev/fastbrowse) covers the statuses, citations,
costs and every option.

`fastbrowse::helper::run_if_requested()` serves and exits when this run of your program is a PDF helper, and
otherwise registers your executable as the helper. Call it before anything else in `main`, since a helper run
must print nothing but its reply. Without it, a run keeps each PDF as a file and tells the run its text was not
read. To run another program as the helper, such as an installed `fastbrowse` of the same version, register
`fastbrowse::helper::HelperCommand::new(path)` with `fastbrowse::helper::register` instead.

`cargo install fastbrowse-cli` installs the `fastbrowse` command, its MCP server and `fastbrowse serve`.
