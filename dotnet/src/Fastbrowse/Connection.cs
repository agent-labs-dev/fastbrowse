#nullable enable

using System;
using System.Collections.Concurrent;
using System.IO;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading;
using System.Threading.Channels;
using System.Threading.Tasks;
using Fastbrowse.Protocol;

namespace Fastbrowse;

/// <summary>
/// JSON-RPC 2.0 over the fastbrowse process's stdin and stdout, one message per line. Both sides may call the other.
/// The reader only queues what it reads, and one loop delivers it in order: notifications, replies, and the end of the
/// connection. A callback that runs long, or waits on disposal, never keeps the reader from seeing the process exit.
/// Requests the server makes are answered on the thread pool.
/// </summary>
internal sealed class Connection
{
    private readonly Stream _output;
    private readonly object _writing = new();
    private readonly ConcurrentDictionary<long, TaskCompletionSource<JsonElement>> _pending = new();
    private readonly Channel<Action> _delivery = Channel.CreateUnbounded<Action>(new UnboundedChannelOptions { SingleReader = true });
    private readonly Action<string, JsonElement> _notify;
    private readonly Func<string, JsonElement, Task<object?>> _serve;
    private long _nextId;
    private Exception? _ended;

    public Connection(Stream output, Action<string, JsonElement> notify, Func<string, JsonElement, Task<object?>> serve)
    {
        _output = output;
        _notify = notify;
        _serve = serve;
        Delivered = Task.Run(DeliverAsync);
    }

    /// <summary>Completes once everything before the end of the connection is delivered.</summary>
    public Task Delivered { get; }

    private async Task DeliverAsync()
    {
        await foreach (Action delivery in _delivery.Reader.ReadAllAsync())
        {
            delivery();
        }
    }

    /// <summary>Reads messages from input until it ends.</summary>
    public async Task ReadAsync(Stream input)
    {
        using var lines = new StreamReader(input, new UTF8Encoding(false));
        string? line;
        while ((line = await lines.ReadLineAsync().ConfigureAwait(false)) is not null)
        {
            if (line.Length > 0 && !Receive(line))
            {
                return;
            }
        }
    }

    /// <summary>Handles one line, and says whether to read on.</summary>
    private bool Receive(string line)
    {
        JsonElement message;
        try
        {
            using JsonDocument document = JsonDocument.Parse(line);
            message = document.RootElement.Clone();
        }
        catch (JsonException)
        {
            // The server keeps everything but messages off this stream, so a line that is not one means the binary is
            // not a fastbrowse server at all. Waiting on it would hang every request.
            End(new FastbrowseException($"fastbrowse sent a line that is not JSON: {line[..Math.Min(line.Length, 200)]}"));
            return false;
        }
        bool hasId = message.TryGetProperty("id", out JsonElement id) && id.ValueKind != JsonValueKind.Null;
        JsonElement parameters = message.TryGetProperty("params", out JsonElement given) ? given : default;
        if (message.TryGetProperty("method", out JsonElement method) && method.ValueKind == JsonValueKind.String)
        {
            string name = method.GetString()!;
            if (!hasId)
            {
                _delivery.Writer.TryWrite(() => _notify(name, parameters));
            }
            else
            {
                _delivery.Writer.TryWrite(() => _ = Task.Run(() => AnswerAsync(id, name, parameters)));
            }
            return true;
        }
        if (!hasId || !id.TryGetInt64(out long number) || !_pending.TryRemove(number, out var waiting))
        {
            return true;
        }
        // After the notifications before it, so a run's events have all been delivered when its reply is.
        if (message.TryGetProperty("error", out JsonElement error) && error.ValueKind == JsonValueKind.Object)
        {
            var refused = new RpcException(error.GetProperty("code").GetInt32(), error.GetProperty("message").GetString() ?? "");
            _delivery.Writer.TryWrite(() => waiting.TrySetException(refused));
        }
        else
        {
            JsonElement result = message.TryGetProperty("result", out JsonElement value) ? value : default;
            _delivery.Writer.TryWrite(() => waiting.TrySetResult(result));
        }
        return true;
    }

    private async Task AnswerAsync(JsonElement id, string method, JsonElement parameters)
    {
        var reply = new JsonObject { ["jsonrpc"] = "2.0", ["id"] = JsonNode.Parse(id.GetRawText()) };
        try
        {
            object? result = await _serve(method, parameters).ConfigureAwait(false);
            reply["result"] = JsonSerializer.SerializeToNode(result, Json.Options);
        }
        catch (Exception e)
        {
            int code = e is RpcException refused ? refused.Code : ProtocolConstants.ErrorInternalError;
            reply["error"] = new JsonObject { ["code"] = code, ["message"] = e.Message };
        }
        Send(reply);
    }

    /// <summary>Calls a method. The task completes with the result, or fails with an <see cref="RpcException"/>.</summary>
    public Task<JsonElement> CallAsync(string method, object? parameters)
    {
        var waiting = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        Exception? reason = Volatile.Read(ref _ended);
        if (reason is not null)
        {
            waiting.SetException(reason);
            return waiting.Task;
        }
        long id = Interlocked.Increment(ref _nextId);
        _pending[id] = waiting;
        var request = new JsonObject
        {
            ["jsonrpc"] = "2.0",
            ["id"] = id,
            ["method"] = method,
            ["params"] = parameters is null ? new JsonObject() : JsonSerializer.SerializeToNode(parameters, Json.Options),
        };
        // Written before this returns, so a later call is written after it.
        Send(request);
        // A process that ended between the check above and the add has no one left to answer.
        reason = Volatile.Read(ref _ended);
        if (reason is not null && _pending.TryRemove(id, out _))
        {
            waiting.TrySetException(reason);
        }
        return waiting.Task;
    }

    private void Send(JsonNode message)
    {
        byte[] line = Encoding.UTF8.GetBytes(message.ToJsonString() + "\n");
        lock (_writing)
        {
            try
            {
                _output.Write(line);
                _output.Flush();
            }
            catch (Exception e) when (e is IOException or ObjectDisposedException)
            {
                // A write to a server that has gone fails here. The request it carried is answered by End, which runs
                // when the process exits, so the error has nothing more to say.
            }
        }
    }

    /// <summary>
    /// The other side is gone. Every request still waiting gets <paramref name="reason"/> once what came before is
    /// delivered, and any made later gets it at once.
    /// </summary>
    public void End(Exception reason)
    {
        if (Interlocked.CompareExchange(ref _ended, reason, null) is not null)
        {
            return;
        }
        var waiting = _pending.Values;
        _pending.Clear();
        _delivery.Writer.TryWrite(() =>
        {
            foreach (var each in waiting)
            {
                each.TrySetException(reason);
            }
        });
        _delivery.Writer.TryComplete();
    }
}

/// <summary>The serializer options every message is read and written with.</summary>
internal static class Json
{
    public static readonly JsonSerializerOptions Options = new()
    {
        DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull,
    };
}
