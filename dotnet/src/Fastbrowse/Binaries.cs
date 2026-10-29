#nullable enable

using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;

namespace Fastbrowse;

/// <summary>Where the fastbrowse executable comes from.</summary>
internal static class Binaries
{
    private const string OwnBinary = "set StartOptions.BinaryPath or FASTBROWSE_BINARY to run a fastbrowse binary of your own";
    private const UnixFileMode Executable = UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute
        | UnixFileMode.GroupRead | UnixFileMode.GroupExecute | UnixFileMode.OtherRead | UnixFileMode.OtherExecute;

    /// <summary>
    /// The executable to start: the caller's own path first, then the one the environment names, then the binary the
    /// package carries, as the app's build laid it out: under <c>runtimes/&lt;rid&gt;/native</c>, or beside the app
    /// when it was published for one runtime.
    /// </summary>
    public static string Resolve(string? path, IReadOnlyDictionary<string, string> environment, string baseDirectory)
    {
        if (!string.IsNullOrEmpty(path))
        {
            return path;
        }
        if (environment.TryGetValue("FASTBROWSE_BINARY", out string? named) && !string.IsNullOrEmpty(named))
        {
            return named;
        }
        string rid = RuntimeIdentifier(RuntimeInformation.OSArchitecture);
        string executable = OperatingSystem.IsWindows() ? "fastbrowse.exe" : "fastbrowse";
        foreach (string candidate in new[]
        {
            Path.Combine(baseDirectory, "runtimes", rid, "native", executable),
            Path.Combine(baseDirectory, executable),
        })
        {
            if (File.Exists(candidate))
            {
                return Runnable(candidate, rid);
            }
        }
        throw new FastbrowseException($"the Fastbrowse package carries no fastbrowse binary for {rid} in {baseDirectory}; {OwnBinary}");
    }

    /// <summary>The .NET runtime identifier the package keeps this platform's binary under.</summary>
    public static string RuntimeIdentifier(Architecture architecture)
    {
        string os = OperatingSystem.IsWindows() ? "win" : OperatingSystem.IsMacOS() ? "osx" : OperatingSystem.IsLinux() ? "linux" : "";
        string cpu = architecture switch
        {
            Architecture.X64 => "x64",
            Architecture.Arm64 => "arm64",
            _ => "",
        };
        string rid = $"{os}-{cpu}";
        if (rid is not ("osx-arm64" or "osx-x64" or "linux-arm64" or "linux-x64" or "win-x64"))
        {
            throw new FastbrowseException($"there is no fastbrowse binary for {RuntimeInformation.OSDescription} {architecture}; {OwnBinary}");
        }
        return rid;
    }

    /// <summary>
    /// The binary, executable. NuGet does not keep a file's execute bit, so it is set here; where the app's directory
    /// cannot be written, the binary runs from a copy in the user cache instead.
    /// </summary>
    public static string Runnable(string binary, string rid)
    {
        if (OperatingSystem.IsWindows() || (File.GetUnixFileMode(binary) & UnixFileMode.UserExecute) != 0)
        {
            return binary;
        }
        try
        {
            File.SetUnixFileMode(binary, Executable);
            return binary;
        }
        catch (Exception e) when (e is UnauthorizedAccessException or IOException)
        {
            string version = typeof(Binaries).Assembly.GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion ?? "unknown";
            string directory = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "fastbrowse", version, rid);
            string copy = Path.Combine(directory, Path.GetFileName(binary));
            if (!File.Exists(copy))
            {
                Directory.CreateDirectory(directory);
                // Written beside its place and moved into it, so a start that stops partway leaves nothing to run.
                string written = Path.Combine(directory, $"binary-{Guid.NewGuid():N}.tmp");
                File.Copy(binary, written);
                File.SetUnixFileMode(written, Executable);
                File.Move(written, copy, overwrite: true);
            }
            return copy;
        }
    }
}
