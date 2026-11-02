#nullable enable

using System;
using System.Collections.Generic;
using System.IO;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Fastbrowse.Protocol;

namespace Fastbrowse;

/// <summary>How to start the fastbrowse process, and the browser every run uses unless the run names another.</summary>
public sealed record StartOptions
{
    /// <summary>
    /// The fastbrowse executable to run. Without it, <c>FASTBROWSE_BINARY</c> names one, and without that it is the
    /// binary this package carries for the platform.
    /// </summary>
    public string? BinaryPath { get; init; }

    /// <summary>Environment variables for the process on top of this one's. Model and browser keys are read from it.</summary>
    public IReadOnlyDictionary<string, string> Environment { get; init; } = new Dictionary<string, string>();

    /// <summary>Receives the process's log, a line at a time. Null writes it to this process's stderr.</summary>
    public Action<string>? Log { get; init; }

    /// <summary>The browser every run drives unless <see cref="RunOptions.Browser"/> names another.</summary>
    public BrowserOptions Browser { get; init; } = new();

    /// <summary>
    /// How long disposing gives the process to exit before it is killed: three times the 10 s the server gives Chrome,
    /// since a server killed first orphans that Chrome.
    /// </summary>
    public TimeSpan GracePeriod { get; init; } = TimeSpan.FromSeconds(30);
}

/// <summary>
/// Which browser a run drives: a Browser Use Cloud browser by default, local Chrome with <see cref="Local"/>, or a
/// browser that is already running through <see cref="CdpUrl"/> or <see cref="CdpPort"/>.
/// </summary>
public sealed record BrowserOptions
{
    /// <summary>Local Chrome instead of a Browser Use Cloud browser.</summary>
    public bool Local { get; init; }

    /// <summary>Which local Chrome, and how. A headed window or a kept profile implies <see cref="Local"/>.</summary>
    public LocalChrome? Chrome { get; init; }

    /// <summary>The Browser Use Cloud profile a cloud browser starts with.</summary>
    public string? CloudProfile { get; init; }

    /// <summary>The DevTools WebSocket URL of a running browser. The run neither starts nor stops it.</summary>
    public string? CdpUrl { get; init; }

    /// <summary>The DevTools port of a browser running on this machine.</summary>
    public int? CdpPort { get; init; }

    /// <summary>Drive a window already open in that browser, and open no tab.</summary>
    public bool Attach { get; init; }

    /// <summary>Part of the URL or title of the window to attach to.</summary>
    public string? TargetMatch { get; init; }

    /// <summary>Bring the run's tab and its window to the front, to watch it.</summary>
    public bool Foreground { get; init; }

    /// <summary>The two-letter country a cloud browser browses from.</summary>
    public string? ProxyCountry { get; init; }

    /// <summary>The page's width and height in CSS pixels.</summary>
    public (int Width, int Height)? Viewport { get; init; }

    /// <summary>Let a cloud browser's window be resized.</summary>
    public bool CloudAllowResizing { get; init; }
}

/// <summary>A file a run may upload.</summary>
public sealed record Attachment(string Name, string MimeType, byte[] Content);

/// <summary>One run's options. <c>new RunOptions()</c> runs the task with the client's browser and no limits.</summary>
public sealed record RunOptions
{
    /// <summary>Replaces the client's browser for this run.</summary>
    public BrowserOptions? Browser { get; init; }

    /// <summary>The address to start on. Without one, the first address is proposed from the task.</summary>
    public string? Start { get; init; }

    /// <summary>Background the run may use but never treats as the task, such as where the request came from.</summary>
    public string? Context { get; init; }

    public IReadOnlyDictionary<string, string>? Inputs { get; init; }

    /// <summary>Files the run may upload.</summary>
    public IReadOnlyList<Attachment>? Attachments { get; init; }

    public Limits? Limits { get; init; }

    /// <summary>What the run may do that it otherwise stops before.</summary>
    public Authorization? Authorization { get; init; }

    /// <summary>
    /// The shape of the data to read, as JSON Schema (draft 2020-12) with an object at its root. The data is
    /// <c>RunResult.Data</c>.
    /// </summary>
    public JsonElement? OutputSchema { get; init; }

    /// <summary>A directory that keeps the files the run downloads.</summary>
    public string? Downloads { get; init; }

    /// <summary>Where to write a video of the run. The server needs ffmpeg for it.</summary>
    public string? Record { get; init; }

    /// <summary>The agent's cursor over visible local Chrome on Linux X11 with the Cua Driver: unset draws it wherever it can be placed, true also brings the window to the front for it, false turns it off.</summary>
    public bool? Cursor { get; init; }

    /// <summary>The secrets the run may type, by name and origin. The values stay with <see cref="ResolveSecret"/>.</summary>
    public IReadOnlyList<SecretRef>? Secrets { get; init; }

    /// <summary>
    /// The value of the secret <c>name</c>, about to be typed on <c>origin</c>, or null when there is none. Called each
    /// time a value is typed and never for an origin the ref does not cover. An exception ends the run with status
    /// error, and what it says is kept out of the result.
    /// </summary>
    public Func<string, string, CancellationToken, ValueTask<string?>>? ResolveSecret { get; init; }

    /// <summary>
    /// Called with the address the run ended on. False keeps the run from <see cref="Status.Complete"/>, and an
    /// exception ends it with status error.
    /// </summary>
    public Func<string, CancellationToken, ValueTask<bool>>? Until { get; init; }

    /// <summary>
    /// Called with the browser event and then each step event, in the order the run produced them, on one thread: a
    /// slow one holds up the run, and one that throws ends it with status error.
    /// </summary>
    public Action<Event>? OnEvent { get; init; }

    /// <summary>
    /// Called with JPEG frames of the active tab while the run goes on. Without it the page is never filmed, and one
    /// that throws ends the run with status error.
    /// </summary>
    public Action<byte[]>? OnFrame { get; init; }
}

/// <summary>One thing a run did: the browser opening, then each step. Exactly one of the two is set.</summary>
public sealed record Event(BrowserEvent? Browser, StepEvent? Step);
