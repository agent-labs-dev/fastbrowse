#nullable enable

using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Fastbrowse.Protocol;
using Xunit;

namespace Fastbrowse.Tests;

/// <summary>
/// The client against the real server, with one of the stand-in runs it carries behind FASTBROWSE_TEST_SERVER in place
/// of the agent, so a run needs no browser and no model.
/// </summary>
public sealed class FastbrowseClientTests : IAsyncLifetime
{
    private readonly List<FastbrowseClient> _started = [];

    /// <summary>FASTBROWSE_TEST_BINARY, or the workspace's debug build.</summary>
    private static string ServerBinary()
    {
        string? named = Environment.GetEnvironmentVariable("FASTBROWSE_TEST_BINARY");
        if (!string.IsNullOrEmpty(named))
        {
            return named;
        }
        string built = Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "../../../../../../target/debug/fastbrowse"));
        Assert.SkipUnless(File.Exists(built), "no fastbrowse binary: run `cargo build -p fastbrowse-cli`, or set FASTBROWSE_TEST_BINARY");
        return built;
    }

    private async Task<FastbrowseClient> Scripted(string run, StartOptions? options = null)
    {
        var client = await FastbrowseClient.StartAsync((options ?? new StartOptions()) with
        {
            BinaryPath = ServerBinary(),
            Environment = new Dictionary<string, string>
            {
                ["FASTBROWSE_TEST_SERVER"] = "scripted_run",
                ["FASTBROWSE_TEST_RUN"] = run,
                // The server refuses a run it has no model keys for, whatever would have run it.
                ["OPENROUTER_API_KEY"] = "unused",
                ["AI_GATEWAY_API_KEY"] = "unused",
                ["BROWSER_USE_API_KEY"] = "unused",
            },
        }, TestContext.Current.CancellationToken);
        _started.Add(client);
        return client;
    }

    public ValueTask InitializeAsync() => ValueTask.CompletedTask;

    public async ValueTask DisposeAsync()
    {
        foreach (var client in _started)
        {
            await client.DisposeAsync();
        }
    }

    [Fact]
    public async Task StartReportsTheBinarysVersion()
    {
        Assert.NotEmpty((await Scripted("echo")).Version);
    }

    [Fact]
    public async Task ARunSendsItsOptionsUnderTheirWireNames()
    {
        var client = await Scripted("echo", new StartOptions { Browser = new BrowserOptions { Local = true, Foreground = true } });
        RunResult result = await client.RunAsync("find the price", new RunOptions
        {
            Start = "https://shop.example/",
            Inputs = new Dictionary<string, string> { ["size"] = "M" },
            Attachments = [new Attachment("a.txt", "text/plain", Encoding.UTF8.GetBytes("hello"))],
            Limits = new Limits { MaxSteps = 5 },
            OutputSchema = JsonDocument.Parse("""{"type": "object", "properties": {"price": {"type": "number"}}}""").RootElement,
            Until = (_, _) => ValueTask.FromResult(true),
        }, TestContext.Current.CancellationToken);
        Assert.Equal(Status.Complete, result.Status);
        Assert.Equal("find the price", result.Answer);
        JsonElement parameters = result.Data!.Value.GetProperty("params");
        Assert.Equal("https://shop.example/", parameters.GetProperty("start").GetString());
        Assert.True(parameters.GetProperty("local").GetBoolean());
        Assert.True(parameters.GetProperty("foreground").GetBoolean());
        Assert.True(parameters.GetProperty("until").GetBoolean());
        Assert.Equal(5, parameters.GetProperty("limits").GetProperty("max_steps").GetInt32());
        // What was not asked for is not sent, so the server applies its own defaults.
        foreach (string key in new[] { "frames", "context", "cdp_port", "secrets" })
        {
            Assert.False(parameters.TryGetProperty(key, out _), $"{key} was sent");
        }
        // The stand-in echoes the bytes the server decoded, as a list of numbers.
        byte[] attachment = result.Data!.Value.GetProperty("attachments")[0].EnumerateArray().Select(each => (byte)each.GetInt32()).ToArray();
        Assert.Equal("hello", Encoding.UTF8.GetString(attachment));
    }

    [Fact]
    public async Task ARunsOwnBrowserReplacesTheClients()
    {
        var client = await Scripted("echo", new StartOptions { Browser = new BrowserOptions { CdpPort = 9222 } });
        RunResult result = await client.RunAsync("t", new RunOptions { Browser = new BrowserOptions { Local = true } }, TestContext.Current.CancellationToken);
        JsonElement parameters = result.Data!.Value.GetProperty("params");
        Assert.False(parameters.TryGetProperty("cdp_port", out _));
        Assert.True(parameters.GetProperty("local").GetBoolean());
    }

    [Fact]
    public async Task EventsArriveInTheOrderTheRunProducedThem()
    {
        var events = new List<Event>();
        await (await Scripted("three_events")).RunAsync("t", new RunOptions { OnEvent = events.Add }, TestContext.Current.CancellationToken);
        Assert.Equal(3, events.Count);
        Assert.Equal("https://live.example/1", events[0].Browser!.LiveUrl);
        Assert.Equal(0, events[1].Step!.Step.Index);
        Assert.Equal(1, events[2].Step!.Step.Index);
    }

    [Fact]
    public async Task AnEventCallbackThatThrowsEndsTheRunWithStatusError()
    {
        int calls = 0;
        RunResult result = await (await Scripted("three_events")).RunAsync("t", new RunOptions
        {
            OnEvent = _ =>
            {
                calls++;
                throw new InvalidOperationException("full disk");
            },
        }, TestContext.Current.CancellationToken);
        Assert.Equal(Status.Error, result.Status);
        Assert.Equal("run/event: full disk", result.Error);
        Assert.Equal(1, calls);
    }

    [Fact]
    public async Task AStatusIsAResultNotAnException()
    {
        RunResult result = await (await Scripted("needs_login")).RunAsync("t", cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal(Status.NeedsLogin, result.Status);
    }

    [Fact]
    public async Task UntilDecidesWhetherTheRunIsComplete()
    {
        string? asked = null;
        RunResult result = await (await Scripted("ends_on_an_order")).RunAsync("t", new RunOptions
        {
            Until = (url, _) =>
            {
                asked = url;
                return ValueTask.FromResult(false);
            },
        }, TestContext.Current.CancellationToken);
        Assert.Equal(Status.Unverified, result.Status);
        Assert.Equal("https://shop.example/orders/17", asked);
    }

    [Fact]
    public async Task ASecretIsResolvedEachTimeItIsTyped()
    {
        int calls = 0;
        RunResult result = await (await Scripted("types_secrets")).RunAsync("t", new RunOptions
        {
            Inputs = new Dictionary<string, string> { ["name"] = "password", ["origin"] = "https://shop.example" },
            Secrets = [new SecretRef { Name = "password", Origins = ["https://shop.example"] }],
            ResolveSecret = (name, origin, _) =>
            {
                Interlocked.Increment(ref calls);
                return ValueTask.FromResult<string?>($"{name}@{origin}");
            },
        }, TestContext.Current.CancellationToken);
        Assert.Equal(2, calls);
        Assert.Equal("password@https://shop.example", result.Data!.Value[0].GetString());
    }

    [Fact]
    public async Task AResolverThatThrowsEndsTheRunWithoutWhatItSaid()
    {
        RunResult result = await (await Scripted("types_secrets")).RunAsync("t", new RunOptions
        {
            Inputs = new Dictionary<string, string> { ["name"] = "password", ["origin"] = "https://shop.example" },
            Secrets = [new SecretRef { Name = "password", Origins = ["https://shop.example"] }],
            ResolveSecret = (_, _, _) => throw new InvalidOperationException("vault said hunter2"),
        }, TestContext.Current.CancellationToken);
        Assert.Equal(Status.Error, result.Status);
        Assert.DoesNotContain("hunter2", result.Error);
    }

    [Fact]
    public async Task FramesArriveAsTheirBytes()
    {
        var frames = new List<byte[]>();
        await (await Scripted("two_frames")).RunAsync("t", new RunOptions { OnFrame = frames.Add }, TestContext.Current.CancellationToken);
        Assert.Equal(2, frames.Count);
        Assert.Equal(256, frames[0].Length);
        Assert.Equal(new byte[] { 0xff, 0xd8, (byte)' ', (byte)'s', (byte)'e', (byte)'c', (byte)'o', (byte)'n', (byte)'d' }, frames[1]);
    }

    [Fact]
    public async Task CancellingStopsTheRunAfterItsBrowserCloses()
    {
        var client = await Scripted("until_cancelled");
        string closed = Path.Combine(Path.GetTempPath(), $"fastbrowse-{Guid.NewGuid():N}");
        using var cancellation = new CancellationTokenSource();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.RunAsync("t", new RunOptions
        {
            Inputs = new Dictionary<string, string> { ["closed"] = closed },
            OnEvent = _ => cancellation.Cancel(),
        }, cancellation.Token));
        Assert.True(File.Exists(closed), "the run returned before its browser closed");
        // The process is still there for the next run.
        RunResult next = await client.RunAsync("t", cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal(Status.Complete, next.Status);
        File.Delete(closed);
    }

    [Fact]
    public async Task ACancelledTokenStartsNoRun()
    {
        var client = await Scripted("request_count");
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.RunAsync("t", cancellationToken: new CancellationToken(true)));
        RunResult result = await client.RunAsync("t", cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal(1, result.Data!.Value.GetInt32());
    }

    [Fact]
    public async Task ASecondRunWhileOneIsActiveIsRefusedAsBusy()
    {
        var client = await Scripted("never_ends");
        var started = new TaskCompletionSource();
        using var cancellation = new CancellationTokenSource();
        Task<RunResult> first = client.RunAsync("t", new RunOptions { OnEvent = _ => started.TrySetResult() }, cancellation.Token);
        await started.Task.WaitAsync(TimeSpan.FromSeconds(10), TestContext.Current.CancellationToken);
        var refused = await Assert.ThrowsAsync<RpcException>(() => client.RunAsync("t", cancellationToken: TestContext.Current.CancellationToken));
        Assert.Equal(ProtocolConstants.ErrorBusy, refused.Code);
        cancellation.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => first);
    }

    [Fact]
    public async Task ARunOnAProcessThatExitsFailsWithProcessExited()
    {
        var client = await Scripted("exits", new StartOptions { Log = _ => { } });
        var exited = await Assert.ThrowsAsync<ProcessExitedException>(() => client.RunAsync("t", cancellationToken: TestContext.Current.CancellationToken));
        Assert.Equal(7, exited.ExitCode);
        await Assert.ThrowsAsync<ProcessExitedException>(() => client.RunAsync("t", cancellationToken: TestContext.Current.CancellationToken));
    }

    [Fact]
    public async Task DisposingWaitsForTheProcessAndCanBeDoneAgain()
    {
        var client = await Scripted("echo");
        await client.DisposeAsync();
        await client.DisposeAsync();
        await Assert.ThrowsAsync<ProcessExitedException>(() => client.RunAsync("t", cancellationToken: TestContext.Current.CancellationToken));
    }

    [Fact]
    public async Task DisposingFromACallbackReturns()
    {
        var client = await Scripted("never_ends", new StartOptions { GracePeriod = TimeSpan.FromSeconds(5) });
        Task<RunResult> run = client.RunAsync("t", new RunOptions
        {
            OnEvent = _ => client.DisposeAsync().AsTask().GetAwaiter().GetResult(),
        }, TestContext.Current.CancellationToken);
        await Assert.ThrowsAnyAsync<Exception>(() => run.WaitAsync(TimeSpan.FromSeconds(15), TestContext.Current.CancellationToken));
        Assert.True(run.IsCompleted);
    }

    [Fact]
    public async Task StructuredDataIsReadIntoTheCallersType()
    {
        RunResult result = await (await Scripted("found")).RunAsync("t", new RunOptions
        {
            Inputs = new Dictionary<string, string> { ["found"] = """{"price": 12.5}""" },
            OutputSchema = JsonDocument.Parse("""{"type": "object", "properties": {"price": {"type": "number"}}, "required": ["price"]}""").RootElement,
        }, TestContext.Current.CancellationToken);
        Assert.Equal(12.5, result.Data!.Value.Deserialize<Found>()!.price);
    }

    private sealed record Found(double price);
}

public sealed class BinariesTests
{
    [Fact]
    public void TheCallersPathComesFirstAndThenTheEnvironments()
    {
        var environment = new Dictionary<string, string> { ["FASTBROWSE_BINARY"] = "/env" };
        Assert.Equal("/mine", Binaries.Resolve("/mine", environment, "/nowhere"));
        Assert.Equal("/env", Binaries.Resolve(null, environment, "/nowhere"));
    }

    [Fact]
    public void ThePackagesBinaryIsFoundWhereTheBuildPutIt()
    {
        string app = Directory.CreateTempSubdirectory().FullName;
        string rid = Binaries.RuntimeIdentifier(System.Runtime.InteropServices.RuntimeInformation.OSArchitecture);
        string executable = OperatingSystem.IsWindows() ? "fastbrowse.exe" : "fastbrowse";
        Assert.Throws<FastbrowseException>(() => Binaries.Resolve(null, new Dictionary<string, string>(), app));
        // A portable app keeps it under runtimes/<rid>/native; one published for a runtime keeps it beside the app.
        string beside = Path.Combine(app, executable);
        File.WriteAllText(beside, "#!/bin/sh\n");
        Assert.Equal(beside, Binaries.Resolve(null, new Dictionary<string, string>(), app));
        string nested = Path.Combine(app, "runtimes", rid, "native", executable);
        Directory.CreateDirectory(Path.GetDirectoryName(nested)!);
        File.WriteAllText(nested, "#!/bin/sh\n");
        Assert.Equal(nested, Binaries.Resolve(null, new Dictionary<string, string>(), app));
        if (!OperatingSystem.IsWindows())
        {
            // NuGet dropped the execute bit; resolving puts it back.
            Assert.NotEqual(UnixFileMode.None, File.GetUnixFileMode(nested) & UnixFileMode.UserExecute);
        }
    }
}
