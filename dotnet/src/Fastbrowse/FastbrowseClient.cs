#nullable enable

using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Fastbrowse.Protocol;

namespace Fastbrowse;

/// <summary>
/// Runs browser tasks with fastbrowse. A client starts the fastbrowse binary as <c>fastbrowse serve --stdio</c> and
/// speaks its JSON-RPC protocol. It runs one task at a time; start one per run that goes on in parallel.
/// </summary>
/// <example>
/// <code>
/// await using var fastbrowse = await FastbrowseClient.StartAsync();
/// RunResult result = await fastbrowse.RunAsync("What is the top story's title?",
///     new RunOptions { Start = "https://news.ycombinator.com/" });
/// Console.WriteLine($"{result.Status} {result.Answer}");
/// </code>
/// </example>
public sealed class FastbrowseClient : IAsyncDisposable
{
    private readonly Process _process;
    private readonly Connection _connection;
    private readonly BrowserOptions _browser;
    private readonly TimeSpan _gracePeriod;
    private readonly Task _exited;
    private readonly ConcurrentDictionary<string, Served> _runs = new();
    private Task? _disposed;

    /// <summary>A run waiting on its reply, and where the server's calls about it go.</summary>
    private sealed class Served(string runId, RunOptions options, CancellationToken cancellation)
    {
        public string RunId { get; } = runId;
        public RunOptions Options { get; } = options;
        public CancellationToken Cancellation { get; } = cancellation;
        /// <summary>What ended the run from this side: the first OnEvent or OnFrame that threw.</summary>
        public volatile string? Failure;
    }

    private FastbrowseClient(Process process, BrowserOptions browser, TimeSpan gracePeriod)
    {
        _process = process;
        _browser = browser;
        _gracePeriod = gracePeriod;
        _connection = new Connection(process.StandardInput.BaseStream, Notify, ServeAsync);
        _exited = Task.Run(async () =>
        {
            await _connection.ReadAsync(process.StandardOutput.BaseStream).ConfigureAwait(false);
            await process.WaitForExitAsync().ConfigureAwait(false);
            _connection.End(new ProcessExitedException(process.ExitCode));
        });
    }

    /// <summary>The version of the fastbrowse binary this client drives.</summary>
    public string Version { get; private set; } = "";

    /// <summary>
    /// Starts a fastbrowse process and checks that it speaks this client's protocol version. Cancelling stops a start
    /// still under way.
    /// </summary>
    public static async Task<FastbrowseClient> StartAsync(StartOptions? options = null, CancellationToken cancellationToken = default)
    {
        options ??= new StartOptions();
        var environment = Environment.GetEnvironmentVariables().Cast<System.Collections.DictionaryEntry>()
            .ToDictionary(entry => (string)entry.Key, entry => (string?)entry.Value ?? "");
        foreach (var (name, value) in options.Environment)
        {
            environment[name] = value;
        }
        string binary = Binaries.Resolve(options.BinaryPath, environment, AppContext.BaseDirectory);
        var start = new ProcessStartInfo(binary)
        {
            ArgumentList = { "serve", "--stdio" },
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = options.Log is not null,
            UseShellExecute = false,
            CreateNoWindow = true,
        };
        foreach (var (name, value) in options.Environment)
        {
            start.Environment[name] = value;
        }
        Process process;
        try
        {
            process = Process.Start(start) ?? throw new FastbrowseException($"could not start {binary}");
        }
        catch (System.ComponentModel.Win32Exception e)
        {
            throw new FastbrowseException($"could not start {binary}: {e.Message}", e);
        }
        if (options.Log is { } log)
        {
            process.ErrorDataReceived += (_, line) =>
            {
                if (line.Data is not null)
                {
                    log(line.Data);
                }
            };
            process.BeginErrorReadLine();
        }
        var client = new FastbrowseClient(process, options.Browser, options.GracePeriod);
        try
        {
            JsonElement reply = await client._connection
                .CallAsync(ProtocolConstants.MethodInitialize, new InitializeParams { ProtocolVersion = ProtocolConstants.Version })
                .WaitAsync(cancellationToken).ConfigureAwait(false);
            var hello = reply.Deserialize<InitializeResult>(Json.Options)!;
            if (hello.ProtocolVersion != ProtocolConstants.Version)
            {
                throw new FastbrowseException($"this client speaks protocol version {ProtocolConstants.Version}, and fastbrowse "
                    + $"{hello.FastbrowseVersion} at {binary} speaks protocol version {hello.ProtocolVersion}: use matching versions of the two");
            }
            client.Version = hello.FastbrowseVersion;
            return client;
        }
        catch
        {
            process.Kill(entireProcessTree: true);
            await client._exited.ConfigureAwait(false);
            throw;
        }
    }

    /// <summary>
    /// Runs a task and returns its result, whatever status it ended in: a run that needs a login or got stuck is read
    /// from <see cref="RunResult.Status"/>, and is not an exception.
    /// </summary>
    /// <exception cref="RpcException">
    /// The server refused the request, which it does before a browser opens: for a bad option, for a missing key, and
    /// with <see cref="ProtocolConstants.ErrorBusy"/> while another run on this client is active.
    /// </exception>
    /// <exception cref="ProcessExitedException">The fastbrowse process is gone.</exception>
    /// <exception cref="OperationCanceledException">
    /// <paramref name="cancellationToken"/> stopped the run. It is thrown once the run's browser has closed. A token that
    /// is already cancelled starts no run.
    /// </exception>
    public async Task<RunResult> RunAsync(string task, RunOptions? options = null, CancellationToken cancellationToken = default)
    {
        options ??= new RunOptions();
        cancellationToken.ThrowIfCancellationRequested();
        string runId = Guid.NewGuid().ToString("N");
        var served = new Served(runId, options, cancellationToken);
        // The server names the run in every message it sends, which is how a run refused as busy hears nothing of the
        // one that is active.
        _runs[runId] = served;
        try
        {
            Task<JsonElement> reply = _connection.CallAsync(ProtocolConstants.MethodRun, Params(runId, task, options.Browser ?? _browser, options));
            // The reply says only that the request was read, and a process that has gone answers the run itself.
            using (cancellationToken.Register(() => Cancel(runId)))
            {
                try
                {
                    JsonElement result = await reply.ConfigureAwait(false);
                    if (served.Failure is { } failed)
                    {
                        // A run can finish before the cancellation reaches it, and its callback failed all the same.
                        return EndedBy(failed);
                    }
                    return result.Deserialize<RunResult>(Json.Options)
                        ?? throw new FastbrowseException("the server answered the run with null");
                }
                catch (RpcException refused) when (refused.Code == ProtocolConstants.ErrorCancelled)
                {
                    if (served.Failure is { } failed)
                    {
                        return EndedBy(failed);
                    }
                    cancellationToken.ThrowIfCancellationRequested();
                    throw;
                }
            }
        }
        finally
        {
            _runs.TryRemove(runId, out _);
        }
    }

    private void Cancel(string runId) => _ = _connection.CallAsync(ProtocolConstants.MethodRunCancel, new RunCancelParams { RunId = runId });

    /// <summary>
    /// Asks the process to shut down, which stops a run under way, and waits until it has exited. A process still
    /// running after the grace period is killed.
    /// </summary>
    public ValueTask DisposeAsync()
    {
        lock (_runs)
        {
            _disposed ??= ShutDownAsync();
        }
        return new ValueTask(_disposed);
    }

    private async Task ShutDownAsync()
    {
        // The reply says nothing the exit does not, and a process that died earlier answers with an error.
        _ = _connection.CallAsync(ProtocolConstants.MethodShutdown, null);
        if (await Task.WhenAny(_exited, Task.Delay(_gracePeriod)).ConfigureAwait(false) != _exited)
        {
            _process.Kill(entireProcessTree: true);
            await _exited.ConfigureAwait(false);
        }
        _process.Dispose();
    }

    private void Notify(string method, JsonElement parameters)
    {
        if (parameters.ValueKind != JsonValueKind.Object || !parameters.TryGetProperty("run_id", out JsonElement id)
            || !_runs.TryGetValue(id.GetString() ?? "", out Served? run) || run.Failure is not null)
        {
            return;
        }
        try
        {
            if (method == ProtocolConstants.ServerMethodRunEvent && run.Options.OnEvent is { } onEvent)
            {
                JsonElement @event = parameters.GetProperty("event");
                onEvent(@event.GetProperty("type").GetString() switch
                {
                    "browser" => new Event(@event.Deserialize<BrowserEvent>(Json.Options), null),
                    "step" => new Event(null, @event.Deserialize<StepEvent>(Json.Options)),
                    var other => throw new FastbrowseException($"an event of unknown type {other}"),
                });
            }
            else if (method == ProtocolConstants.ServerMethodRunFrame && run.Options.OnFrame is { } onFrame)
            {
                onFrame(Convert.FromBase64String(parameters.GetProperty("frame").GetString() ?? ""));
            }
        }
        catch (Exception e)
        {
            run.Failure = $"{method}: {e.Message}";
            Cancel(run.RunId);
        }
    }

    private async Task<object?> ServeAsync(string method, JsonElement parameters)
    {
        if (method == ProtocolConstants.ServerMethodRunUntil)
        {
            var asked = parameters.Deserialize<RunUntilParams>(Json.Options)!;
            if (!_runs.TryGetValue(asked.RunId, out Served? run) || run.Options.Until is not { } until)
            {
                throw new FastbrowseException($"run '{asked.RunId}' has no until callback waiting");
            }
            return await until(asked.Url, run.Cancellation).ConfigureAwait(false);
        }
        if (method == ProtocolConstants.ServerMethodSecretsResolve)
        {
            var asked = parameters.Deserialize<SecretsResolveParams>(Json.Options)!;
            if (!_runs.TryGetValue(asked.RunId, out Served? run) || run.Options.ResolveSecret is not { } resolve)
            {
                throw new FastbrowseException($"run '{asked.RunId}' has no secret resolver waiting");
            }
            try
            {
                return await resolve(asked.Name, asked.Origin, run.Cancellation).ConfigureAwait(false);
            }
            catch (Exception)
            {
                // What a resolver throws may quote the value, so it stays in this process.
                throw new FastbrowseException($"the resolver threw for the secret '{asked.Name}'");
            }
        }
        throw new RpcException(ProtocolConstants.ErrorMethodNotFound, $"unknown method '{method}'");
    }

    private static RunParams Params(string runId, string task, BrowserOptions browser, RunOptions options) => new()
    {
        RunId = runId,
        Task = task,
        Start = options.Start,
        Context = options.Context,
        Inputs = options.Inputs,
        Attachments = options.Attachments?.Select(each => new RunAttachment
        {
            Name = each.Name,
            MimeType = each.MimeType,
            Content = each.Content,
        }).ToList(),
        Limits = options.Limits,
        Authorization = options.Authorization,
        OutputSchema = options.OutputSchema,
        Secrets = options.Secrets,
        // The wire carries whether a callback is held, and the callback stays here.
        Frames = options.OnFrame is not null ? true : null,
        Until = options.Until is not null ? true : null,
        Downloads = options.Downloads,
        Record = options.Record,
        Cursor = options.Cursor ? true : null,
        Local = browser.Local ? true : null,
        Chrome = browser.Chrome,
        CloudProfile = browser.CloudProfile,
        CdpUrl = browser.CdpUrl,
        CdpPort = browser.CdpPort,
        Attach = browser.Attach ? true : null,
        TargetMatch = browser.TargetMatch,
        Foreground = browser.Foreground ? true : null,
        ProxyCountry = browser.ProxyCountry,
        Viewport = browser.Viewport is var (width, height) ? new List<long> { width, height } : null,
    };

    /// <summary>The result of a run that a throwing callback ended, in the shape the server gives one.</summary>
    private static RunResult EndedBy(string failure) => new()
    {
        Status = Status.Error,
        Evidence = [],
        Steps = [],
        Cost = new CostBreakdown { Lines = [] },
        Artifacts = [],
        Citations = [],
        Error = failure,
        Recordings = [],
    };
}
