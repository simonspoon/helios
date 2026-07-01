using System.Diagnostics;
using System.Text.Json;
using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// Drives the built helios-roslyn.dll as a real child process, the same way the
/// Rust side will (arch.md §2 invocation shape), and parses its NDJSON output.
/// </summary>
internal static class HelperHarness
{
    public static readonly string HelperDll =
        Path.Combine(AppContext.BaseDirectory, "helios-roslyn.dll");

    public static string Fixture(string name) =>
        Path.Combine(AppContext.BaseDirectory, "fixtures", name);

    public sealed record RunResult(int ExitCode, string[] StdoutLines, string Stderr);

    public sealed record Definition(string Docid, string Name, string Kind, string File, int StartLine, int StartCol);

    public sealed record Reference(string Docid, string File, int Line, int Col, bool IsDefinition);

    public sealed record Records(List<Definition> Definitions, List<Reference> References);

    /// <summary>Runs `dotnet helios-roslyn.dll &lt;args&gt;`; fails the test if it does not exit on its own (P2-M4).</summary>
    public static RunResult Run(params string[] args)
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

    /// <summary>Runs analyze on a fixture, asserts success, and parses the record stream.</summary>
    public static Records Analyze(params string[] args)
    {
        var run = Run(["analyze", .. args]);
        Assert.True(run.ExitCode == 0, $"analyze failed (exit {run.ExitCode}): {run.Stderr}");

        var definitions = new List<Definition>();
        var references = new List<Reference>();
        foreach (var line in run.StdoutLines)
        {
            using var doc = JsonDocument.Parse(line); // every stdout line is one JSON object
            var record = doc.RootElement;
            switch (record.GetProperty("type").GetString())
            {
                case "definition":
                    definitions.Add(new Definition(
                        record.GetProperty("docid").GetString()!,
                        record.GetProperty("name").GetString()!,
                        record.GetProperty("kind").GetString()!,
                        record.GetProperty("file").GetString()!,
                        record.GetProperty("start_line").GetInt32(),
                        record.GetProperty("start_col").GetInt32()));
                    break;
                case "reference":
                    references.Add(new Reference(
                        record.GetProperty("docid").GetString()!,
                        record.GetProperty("file").GetString()!,
                        record.GetProperty("line").GetInt32(),
                        record.GetProperty("col").GetInt32(),
                        record.GetProperty("is_definition").GetBoolean()));
                    break;
            }
        }
        return new Records(definitions, references);
    }

    /// <summary>1-based line number of the first fixture-source line containing <paramref name="marker"/>.</summary>
    public static int LineOf(string fixtureDir, string file, string marker)
    {
        var lines = File.ReadAllLines(Path.Combine(fixtureDir, file));
        for (var i = 0; i < lines.Length; i++)
        {
            if (lines[i].Contains(marker, StringComparison.Ordinal))
            {
                return i + 1;
            }
        }
        throw new InvalidOperationException($"marker not found in {file}: {marker}");
    }

    /// <summary>Restores an MSBuild fixture project so design-time builds have their assets file.</summary>
    public static void Restore(string projectPath)
    {
        var psi = new ProcessStartInfo("dotnet")
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
        };
        psi.ArgumentList.Add("restore");
        psi.ArgumentList.Add(projectPath);

        using var process = Process.Start(psi)!;
        var stdout = process.StandardOutput.ReadToEndAsync();
        var stderr = process.StandardError.ReadToEndAsync();
        Assert.True(process.WaitForExit(120_000), "dotnet restore did not finish within 120s");
        Assert.True(process.ExitCode == 0, $"dotnet restore failed: {stdout.Result}\n{stderr.Result}");
    }
}
