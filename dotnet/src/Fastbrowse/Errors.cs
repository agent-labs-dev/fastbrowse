#nullable enable

using System;
using Fastbrowse.Protocol;

namespace Fastbrowse;

/// <summary>Every error this library throws, so one catch tells its failures from the caller's own.</summary>
public class FastbrowseException : Exception
{
    public FastbrowseException(string message, Exception? inner = null) : base(message, inner) { }
}

/// <summary>
/// The server's error reply to a request. <see cref="Code"/> is one of the <c>ProtocolConstants.Error*</c> codes:
/// <see cref="ProtocolConstants.ErrorBusy"/> while another run on the same client is active,
/// <see cref="ProtocolConstants.ErrorConfiguration"/> for a missing key, <see cref="ProtocolConstants.ErrorInvalidParams"/>
/// for an option the server refuses.
/// </summary>
public sealed class RpcException : FastbrowseException
{
    public RpcException(int code, string message) : base(message) => Code = code;

    public int Code { get; }
}

/// <summary>The fastbrowse process is gone, so what was waiting on it, or is asked of it later, has no answer.</summary>
public sealed class ProcessExitedException : FastbrowseException
{
    public ProcessExitedException(int exitCode) : base($"the fastbrowse process exited with code {exitCode}") =>
        ExitCode = exitCode;

    /// <summary>The process's exit code; on Unix a process killed by a signal reports 128 plus the signal's number.</summary>
    public int ExitCode { get; }
}
