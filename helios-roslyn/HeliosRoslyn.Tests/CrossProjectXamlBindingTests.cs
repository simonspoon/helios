using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// A code-behind page can live in a project referenced by the one being analyzed
/// (App -> Lib via ProjectReference). Regression coverage for a crash where
/// `Compilation.GetTypeByMetadataName` resolved the referencing compilation's view
/// of the referenced project's code-behind type, whose syntax tree that compilation
/// doesn't own — `GetSemanticModel` threw instead of the binding resolving.
/// </summary>
public class CrossProjectXamlBindingTests
{
    private static readonly string Fixture = HelperHarness.Fixture("crossproject");

    static CrossProjectXamlBindingTests() =>
        HelperHarness.Restore(Path.Combine(Fixture, "App", "App.csproj"));

    private static readonly Lazy<HelperHarness.Records> Output =
        new(() => HelperHarness.Analyze("--root", Fixture));

    [Fact] // App discovers Lib.csproj first alphabetically, so App's compilation is the one
           // that resolves LibPage via the ProjectReference; analyze must not crash on that
    public void AnalyzeSucceedsAcrossProjectReference()
    {
        Assert.NotEmpty(Output.Value.Definitions);
    }

    [Fact] // no x:DataType on the page: BindingContext is assigned in Lib's own code-behind
    public void CrossProjectBindingContext_ResolvesToLibViewModelProperty()
    {
        var line = HelperHarness.LineOf(Fixture, "Lib/ViewModels/LibViewModel.cs", "public string Heading");
        var heading = Assert.Single(Output.Value.Definitions,
            d => d.File == "Lib/ViewModels/LibViewModel.cs" && d.StartLine == line).Docid;

        var bindingLine = HelperHarness.LineOf(Fixture, "Lib/Views/LibPage.xaml", "{Binding Heading}");
        var binding = Assert.Single(Output.Value.References,
            r => r.File == "Lib/Views/LibPage.xaml" && r.Line == bindingLine);

        Assert.Equal(heading, binding.Docid);
    }
}
