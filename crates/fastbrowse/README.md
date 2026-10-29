# fastbrowse

A browser agent that picks instead of generating. Every page is indexed into the controls it actually has, and a
choice model picks one of them. An LLM plans, reads and writes prose. Code owns authorization gates, secret
resolution, cost limits, and whether a run may call itself finished.

```toml
[dependencies]
fastbrowse = "0.9"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use fastbrowse::{Limits, RunTask, run_task};

#[tokio::main]
async fn main() -> Result<(), fastbrowse::RunError> {
    let result = run_task(
        "What is the top story's title?",
        RunTask {
            start: Some("https://news.ycombinator.com/".into()),
            limits: Some(Limits { max_dollars: Some(0.25), ..Limits::default() }),
            ..RunTask::default()
        },
    )
    .await?;
    println!("{:?}: {}", result.status, result.answer.unwrap_or_default());
    Ok(())
}
```

Model keys come from the environment: `TYPESAFE_API_KEY` for Jev and `OPENROUTER_API_KEY` for the LLM. The run
starts local Chrome, or a Browser Use Cloud browser when `browser_api_key` is given. The [project README](https://github.com/agent-labs-dev/fastbrowse) covers the statuses, citations,
costs and every option.

`cargo install fastbrowse-cli` installs the `fastbrowse` command, its MCP server and `fastbrowse serve`.
