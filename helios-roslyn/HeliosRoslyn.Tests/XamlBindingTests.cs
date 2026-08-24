using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// The XAML pass: `{Binding}` paths in .xaml resolved against the project's
/// compilation and emitted on the same docids as the C# passes. Each case
/// asserts the binding's reference record carries the *intended* member's
/// docid — the resolutions a name match against the markup could not make.
/// </summary>
public class XamlBindingTests
{
    private static readonly string MauiFixture = HelperHarness.Fixture("maui");

    static XamlBindingTests() =>
        HelperHarness.Restore(Path.Combine(MauiFixture, "App.csproj"));

    private static readonly Lazy<HelperHarness.Records> Output =
        new(() => HelperHarness.Analyze("--root", MauiFixture));

    private static string DocidOf(string file, string marker)
    {
        var line = HelperHarness.LineOf(MauiFixture, file, marker);
        return Assert.Single(Output.Value.Definitions, d => d.File == file && d.StartLine == line).Docid;
    }

    /// <summary>The single binding reference on a markup line.</summary>
    private static HelperHarness.Reference BindingAt(string file, string marker)
    {
        var line = HelperHarness.LineOf(MauiFixture, file, marker);
        return Assert.Single(Output.Value.References, r => r.File == file && r.Line == line);
    }

    [Fact] // x:DataType on the root page anchors its bindings to that ViewModel
    public void RootDataType_BindsToViewModelProperty()
    {
        var query = DocidOf("ViewModels/MainViewModel.cs", "public string Query");

        Assert.Equal(query, BindingAt("Views/MainPage.xaml", "{Binding Query, Mode=TwoWay}").Docid);
    }

    [Fact] // the bound member is declared on the base class, not on the x:DataType
    public void InheritedProperty_ResolvesToDeclaringBaseClass()
    {
        var isBusy = DocidOf("ViewModels/MainViewModel.cs", "public bool IsBusy");

        var binding = BindingAt("Views/MainPage.xaml", "{Binding IsBusy}");
        Assert.Equal(isBusy, binding.Docid);
        Assert.Contains("BaseViewModel", binding.Docid);
    }

    [Fact] // MainViewModel.Title and SearchResult.Title share a name; the DataTemplate's x:DataType separates them
    public void SameNameUnderDataTemplate_ResolvesToItemType()
    {
        var pageTitle = DocidOf("ViewModels/MainViewModel.cs", "public string Title { get; set; }");
        var itemTitle = DocidOf("ViewModels/MainViewModel.cs", "public string Title { get; init; }");
        Assert.NotEqual(pageTitle, itemTitle);

        Assert.Equal(pageTitle, BindingAt("Views/MainPage.xaml", "Title=\"{Binding Title}\"").Docid);
        Assert.Equal(itemTitle, BindingAt("Views/MainPage.xaml", "<Label Text=\"{Binding Title}\"").Docid);
    }

    [Fact] // no x:DataType on the template: the item type comes from the enclosing ItemsSource
    public void DataTemplateWithoutDataType_InfersItemTypeFromItemsSource()
    {
        var itemTitle = DocidOf("ViewModels/MainViewModel.cs", "public string Title { get; init; }");

        Assert.Equal(itemTitle, BindingAt("Views/MainPage.xaml", "{Binding Path=Title}").Docid);
    }

    [Fact] // a dotted path reports every segment, each against the previous segment's type
    public void DottedPath_EmitsAReferencePerSegment()
    {
        var profile = DocidOf("ViewModels/MainViewModel.cs", "public Profile Profile");
        var displayName = DocidOf("ViewModels/MainViewModel.cs", "public string DisplayName");

        var line = HelperHarness.LineOf(MauiFixture, "Views/MainPage.xaml", "{Binding Profile.DisplayName}");
        var segments = Output.Value.References
            .Where(r => r.File == "Views/MainPage.xaml" && r.Line == line)
            .OrderBy(r => r.Col)
            .ToList();

        Assert.Equal(new[] { profile, displayName }, segments.Select(r => r.Docid).ToArray());
    }

    [Fact] // a binding nested inside another extension's argument is still found
    public void NestedMarkupExtension_BindingIsResolved()
    {
        var searchCommand = DocidOf("ViewModels/MainViewModel.cs", "public ICommand? SearchCommand");
        var query = DocidOf("ViewModels/MainViewModel.cs", "public string Query");

        var line = HelperHarness.LineOf(MauiFixture, "Views/MainPage.xaml", "CommandParameter={Binding Query}");
        var bindings = Output.Value.References
            .Where(r => r.File == "Views/MainPage.xaml" && r.Line == line)
            .OrderBy(r => r.Col)
            .ToList();

        Assert.Equal(new[] { searchCommand, query }, bindings.Select(r => r.Docid).ToArray());
    }

    [Fact] // no x:DataType anywhere: the context is whatever the code-behind assigns
    public void NoDataType_FallsBackToCodeBehindBindingContext()
    {
        var heading = DocidOf("ViewModels/DetailViewModel.cs", "public string Heading");

        Assert.Equal(heading, BindingAt("Views/DetailPage.xaml", "{Binding Heading}").Docid);
    }

    [Fact] // positions address the path segment inside the attribute value, not the attribute or the line
    public void ReportedPosition_PointsAtTheBoundIdentifier()
    {
        var markup = File.ReadAllLines(Path.Combine(MauiFixture, "Views/MainPage.xaml"));

        foreach (var reference in Output.Value.References.Where(r => r.File == "Views/MainPage.xaml"))
        {
            var name = reference.Docid[(reference.Docid.LastIndexOf('.') + 1)..];
            var line = markup[reference.Line - 1];
            Assert.Equal(name, line.Substring(reference.Col - 1, name.Length));
        }
    }

    [Fact] // markup is never reported as a definition, and never for an unresolvable path
    public void XamlReferencesAreUsagesOnly()
    {
        Assert.DoesNotContain(Output.Value.Definitions, d => d.File.EndsWith(".xaml", StringComparison.Ordinal));
        Assert.All(
            Output.Value.References.Where(r => r.File.EndsWith(".xaml", StringComparison.Ordinal)),
            r => Assert.False(r.IsDefinition));
    }
}
