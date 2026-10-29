# fastbrowse for .NET

A browser agent that picks instead of generating. Every page is indexed into the controls it actually has, and a
choice model picks one of them. An LLM plans, reads and writes prose. Code owns authorization gates, secret
resolution, cost limits, and whether a run may call itself finished.

```sh
dotnet add package Fastbrowse
```

```csharp
using Fastbrowse;
using Fastbrowse.Protocol;

await using var fastbrowse = await FastbrowseClient.StartAsync();
RunResult result = await fastbrowse.RunAsync("What is the top story's title?", new RunOptions
{
    Start = "https://news.ycombinator.com/",
    Limits = new Limits { MaxDollars = 0.25 },
});
Console.WriteLine($"{result.Status} {result.Answer}");
```

`StartAsync` runs the fastbrowse binary as `fastbrowse serve --stdio`. The package carries the binary for macOS
(arm64, x64), Linux with glibc (arm64, x64) and Windows (x64) under `runtimes/<rid>/native`; `StartOptions.BinaryPath`
or `FASTBROWSE_BINARY` runs a binary of your own instead. Model keys come from the environment, as the fastbrowse
command reads them: `TYPESAFE_API_KEY` for Jev and `OPENROUTER_API_KEY` for the LLM. A run uses a Browser Use Cloud
browser with `BROWSER_USE_API_KEY`, or local Chrome with `new BrowserOptions { Local = true }`. .NET 10 or newer.

A run ends in a `Status`; a run that needs a login or got stuck is read from the result, not thrown. Cancelling the
`CancellationToken` stops a run, and `RunAsync` throws `OperationCanceledException` once its browser has closed. One
client runs one task at a time: a second `RunAsync` while one is active throws an `RpcException` whose `Code` is
`ProtocolConstants.ErrorBusy`. Start one client per concurrent run.

`RunOptions` carries the rest: `OutputSchema` for structured data, read back from `result.Data`; `Secrets` and
`ResolveSecret`, whose values stay in your process and are asked for as they are typed; `Until`, a check on the address
a run ends on; and `OnEvent` and `OnFrame` to follow a run as it goes. The
[project README](https://github.com/agent-labs-dev/fastbrowse) covers the statuses, citations and costs.
