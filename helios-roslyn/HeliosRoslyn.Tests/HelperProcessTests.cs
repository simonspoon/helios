using System.Text.Json;
using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// Core helper contract: ping, loose-fixture golden output, clean failure.
/// </summary>
public class HelperProcessTests
{
    private static readonly string LooseFixture = HelperHarness.Fixture("loose");

    private static readonly string GoldenLoose =
        Path.Combine(AppContext.BaseDirectory, "golden", "loose.ndjson");

    [Fact] // P2-M2
    public void Ping_EmitsCapabilityRecord_AndExitsZero()
    {
        var run = HelperHarness.Run("ping");

        Assert.Equal(0, run.ExitCode);
        var line = Assert.Single(run.StdoutLines);

        using var doc = JsonDocument.Parse(line);
        var record = doc.RootElement;
        Assert.Equal("ping", record.GetProperty("type").GetString());
        Assert.True(record.GetProperty("available").GetBoolean());
        // helios gates semantic mode on this: a helper that cannot report a
        // contract version is refused before analyze.
        Assert.True(record.GetProperty("protocol_version").GetInt32() >= 1);
        Assert.False(string.IsNullOrEmpty(record.GetProperty("dotnet_version").GetString()));
        Assert.False(string.IsNullOrEmpty(record.GetProperty("roslyn_version").GetString()));
    }

    [Fact] // P2-M3 + P2-M5: loose .cs fixture, no csproj/sln, golden-file NDJSON
    public void Analyze_LooseFixture_MatchesGolden()
    {
        var run = HelperHarness.Run("analyze", "--root", LooseFixture);

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

    [Fact] // --files: the caller-supplied list is the file vocabulary, overriding the bin/obj heuristic
    public void Analyze_FilesList_LimitsOutputToListedFiles()
    {
        var list = Path.Combine(Path.GetTempPath(), $"helios-files-{Guid.NewGuid():N}.txt");
        File.WriteAllText(list, "Person.cs\n");
        try
        {
            var records = HelperHarness.Analyze("--root", LooseFixture, "--files", list);

            Assert.Contains(records.Definitions, d => d.File == "Person.cs");
            Assert.DoesNotContain(records.Definitions, d => d.File == "Program.cs");
            Assert.DoesNotContain(records.References, r => r.File == "Program.cs");
        }
        finally
        {
            File.Delete(list);
        }
    }

    [Fact] // P2-S1: unloadable root → non-zero exit, stderr diagnostic, stdout NDJSON-only
    public void Analyze_UnloadableRoot_FailsCleanly()
    {
        var missing = Path.Combine(AppContext.BaseDirectory, "no-such-dir-" + Guid.NewGuid().ToString("N"));

        var run = HelperHarness.Run("analyze", "--root", missing);

        Assert.NotEqual(0, run.ExitCode);
        Assert.False(string.IsNullOrWhiteSpace(run.Stderr));
        foreach (var line in run.StdoutLines)
        {
            using var _ = JsonDocument.Parse(line); // stdout must still be pure NDJSON
        }
    }
}
