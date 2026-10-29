#nullable enable

using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Xunit;

namespace Fastbrowse.Tests;

public sealed class ConnectionTests
{
    [Fact]
    public async Task EveryCallMadeAsTheProcessEndsIsAnswered()
    {
        // Calls racing the end of the connection: each must complete, with the end's reason, rather than wait forever.
        for (int round = 0; round < 500; round++)
        {
            var connection = new Connection(Stream.Null, (_, _) => { }, (_, _) => Task.FromResult<object?>(null));
            var calls = new List<Task<JsonElement>>();
            using var go = new ManualResetEventSlim();
            Task calling = Task.Run(() =>
            {
                go.Wait(TestContext.Current.CancellationToken);
                for (int i = 0; i < 20; i++)
                {
                    calls.Add(connection.CallAsync("run", null));
                }
            }, TestContext.Current.CancellationToken);
            Task ending = Task.Run(() =>
            {
                go.Wait(TestContext.Current.CancellationToken);
                connection.End(new ProcessExitedException(1));
            }, TestContext.Current.CancellationToken);
            go.Set();
            await Task.WhenAll(calling, ending);
            Task all = Task.WhenAll(calls.ToArray());
            Task finished = await Task.WhenAny(all, Task.Delay(TimeSpan.FromSeconds(5), TestContext.Current.CancellationToken));
            Assert.True(finished == all, $"a call made in round {round} was never answered");
            Assert.All(calls, call => Assert.IsType<ProcessExitedException>(call.Exception!.InnerException));
        }
    }
}
