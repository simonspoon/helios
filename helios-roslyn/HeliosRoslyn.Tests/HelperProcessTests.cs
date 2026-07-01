using System.Diagnostics;
using System.Text.Json;
using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// Drives the built helios-roslyn.dll as a real child process, the same way the
/// Rust side will (arch.md §2 invocation shape).
/// </summary>
public class HelperProcessTests
{
    private static readonly string HelperDll =
        Path.Combine(AppContext.BaseDirectory, "helios-roslyn.dll");

    private static readonly string LooseFixture =
        Path.Combine(AppContext.BaseDirectory, "fixtures", "loose");

    private static readonly string GoldenLoose =
        Path.Combine(AppContext.BaseDirectory, "golden", "loose.ndjson");

    private sealed record RunResult(int ExitCode, string[] StdoutLines, string Stderr);

    /// <summary>Runs `dotnet helios-roslyn.dll &lt;args&gt;`; fails the test if it does not exit on its own (P2-M4).</summary>
    private static RunResult RunHelper(params string[] args)
    {
        var psi = new ProcessStartInfo("dotnet")
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
        };
        psi.ArgumentList.Add(HelperDll);
        foreach (var a in args)
        {
            psi.ArgumentList.Add(a);
        }

        using var process = Process.Start(psi)!;
        var stdout = process.StandardOutput.ReadToEndAsync();
        var stderr = process.StandardError.ReadToEndAsync();

        // One-shot contract: the process must terminate by itself, no lifecycle.
        Assert.True(process.WaitForExit(120_000), "helper did not exit on its own within 120s");

        var lines = stdout.Result
            .Replace("\r\n", "\n")
            .Split('\n', StringSplitOptions.RemoveEmptyEntries);
        return new RunResult(process.ExitCode, lines, stderr.Result);
    }

    [Fact] // P2-M2
    public void Ping_EmitsCapabilityRecord_AndExitsZero()
    {
        var run = RunHelper("ping");

        Assert.Equal(0, run.ExitCode);
        var line = Assert.Single(run.StdoutLines);

        using var doc = JsonDocument.Parse(line);
        var record = doc.RootElement;
        Assert.Equal("ping", record.GetProperty("type").GetString());
        Assert.True(record.GetProperty("available").GetBoolean());
        Assert.False(string.IsNullOrEmpty(record.GetProperty("dotnet_version").GetString()));
        Assert.False(string.IsNullOrEmpty(record.GetProperty("roslyn_version").GetString()));
    }

    [Fact] // P2-M3 + P2-M5: loose .cs fixture, no csproj/sln, golden-file NDJSON
    public void Analyze_LooseFixture_MatchesGolden()
    {
        var run = RunHelper("analyze", "--root", LooseFixture);

        Assert.Equal(0, run.ExitCode);
        foreach (var line in run.StdoutLines)
        {
            using var doc = JsonDocument.Parse(line); // every stdout line is one JSON object
            Assert.True(doc.RootElement.TryGetProperty("type", out _));
        }

        // No ordering guarantee on the wire (arch.md §1) — sort both sides.
        var actual = run.StdoutLines.OrderBy(l => l, StringComparer.Ordinal).ToArray();
        var expected = File.ReadAllLines(GoldenLoose)
            .Where(l => l.Length > 0)
            .OrderBy(l => l, StringComparer.Ordinal)
            .ToArray();

        Assert.Equal(expected, actual);
    }

    [Fact] // P2-S1: unloadable root → non-zero exit, stderr diagnostic, stdout NDJSON-only
    public void Analyze_UnloadableRoot_FailsCleanly()
    {
        var missing = Path.Combine(AppContext.BaseDirectory, "no-such-dir-" + Guid.NewGuid().ToString("N"));

        var run = RunHelper("analyze", "--root", missing);

        Assert.NotEqual(0, run.ExitCode);
        Assert.False(string.IsNullOrWhiteSpace(run.Stderr));
        foreach (var line in run.StdoutLines)
        {
            using var _ = JsonDocument.Parse(line); // stdout must still be pure NDJSON
        }
    }
}
