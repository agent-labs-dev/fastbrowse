# fastbrowse for Go

A browser agent that picks instead of generating. Every page is indexed into the controls it actually has, and a
choice model picks one of them. An LLM plans, reads and writes prose. Code owns authorization gates, secret
resolution, cost limits, and whether a run may call itself finished.

```sh
go get github.com/agent-labs-dev/fastbrowse/go
```

```go
import fastbrowse "github.com/agent-labs-dev/fastbrowse/go"

client, err := fastbrowse.Start(ctx, fastbrowse.Options{})
if err != nil {
	return err
}
defer client.Close(context.Background())

maxDollars := 0.25
result, err := client.Run(ctx, "What is the top story's title?", fastbrowse.RunOptions{
	Start:  "https://news.ycombinator.com/",
	Limits: &fastbrowse.Limits{MaxDollars: &maxDollars},
})
if err != nil {
	return err
}
fmt.Println(result.Status, *result.Answer)
```

`Start` runs the fastbrowse binary as `fastbrowse serve --stdio`. The first start downloads the binary of the
release this module was published with into the user cache directory, and refuses an archive whose SHA-256 is not
the one the module pins. `Options.BinaryPath` or `FASTBROWSE_BINARY` runs a binary of your own instead. Binaries are
published for macOS (arm64, x64), Linux with glibc (arm64, x64) and Windows (x64).

Model keys come from the environment, as the fastbrowse command reads them: `TYPESAFE_API_KEY` for Jev and
`OPENROUTER_API_KEY` for the LLM. A run uses a Browser Use Cloud browser with `BROWSER_USE_API_KEY`, or local Chrome
with `BrowserOptions.Local`.

A run ends in a `Status`; a run that needs a login or got stuck is read from the result, not returned as an error.
Cancelling `ctx` stops a run, and `Run` returns once its browser has closed. One `Client` runs one task at a time:
a second `Run` while one is active gets an `*RPCError` with `ErrorCodeBusy`. Start one client per concurrent run.

`RunOptions` carries the rest: `OutputSchema` for structured data, read back with `result.DecodeData`; `Secrets`,
whose values stay in your process and are asked for as they are typed; `Until`, a check on the address a run ends on;
and `OnEvent` and `OnFrame` to follow a run as it goes. The
[project README](https://github.com/agent-labs-dev/fastbrowse) covers the statuses, citations and costs.
