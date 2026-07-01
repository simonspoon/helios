using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// P2-M6: analyze on a .csproj/.sln fixture loads through MSBuildWorkspace and emits
/// Definitions/References. Proof the MSBuild path (not the adhoc file walk) ran: the
/// fixture's Excluded.cs is removed from the compile set by App.csproj, so its symbols
/// must be absent — the adhoc walk would have picked the file up.
/// </summary>
public class MsBuildProjectTests
{
    private static readonly string ProjectFixture = HelperHarness.Fixture("project");

    static MsBuildProjectTests() =>
        HelperHarness.Restore(Path.Combine(ProjectFixture, "App.csproj"));

    [Fact] // discovery path: --root containing a .sln → OpenSolutionAsync
    public void Analyze_SlnRoot_LoadsViaMsBuild()
    {
        var records = HelperHarness.Analyze("--root", ProjectFixture);

        AssertMsBuildSemantics(records);
    }

    [Fact] // explicit path: --project <csproj> → OpenProjectAsync
    public void Analyze_ExplicitProjectArg_LoadsViaMsBuild()
    {
        var records = HelperHarness.Analyze("--root", ProjectFixture, "--project", "App.csproj");

        AssertMsBuildSemantics(records);
    }

    private static void AssertMsBuildSemantics(HelperHarness.Records records)
    {
        // Definitions emitted for the compiled sources.
        Assert.Contains(records.Definitions, d => d is { Name: "Widget", Kind: "class", File: "Lib.cs" });
        var ping = Assert.Single(records.Definitions, d => d is { Name: "Ping", Kind: "fn" });

        // MSBuild honored <Compile Remove="Excluded.cs"> — adhoc would still see Ghost.
        Assert.DoesNotContain(records.Definitions, d => d.Name is "Ghost" or "Haunt");
        Assert.DoesNotContain(records.References, r => r.File == "Excluded.cs");

        // Cross-file reference resolved by exact docid.
        var call = HelperHarness.LineOf(ProjectFixture, "App.cs", "widget.Ping()");
        var usage = Assert.Single(records.References, r => r is { File: "App.cs", IsDefinition: false } && r.Line == call);
        Assert.Equal(ping.Docid, usage.Docid);
    }
}
