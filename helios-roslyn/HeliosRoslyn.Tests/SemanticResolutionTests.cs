using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// P2-M7: the semantic wins tree-sitter cannot deliver. Each case asserts the reference
/// record's docid equals the *intended* definition's docid — exact resolution, no
/// name-collapse. Call sites are addressed by fixture source line (one call per line).
/// </summary>
public class SemanticResolutionTests
{
    private static readonly string SemanticFixture = HelperHarness.Fixture("semantic");

    // One analyze run shared by all cases.
    private static readonly Lazy<HelperHarness.Records> Output =
        new(() => HelperHarness.Analyze("--root", SemanticFixture));

    private static HelperHarness.Definition DefinitionAt(string file, string marker)
    {
        var line = HelperHarness.LineOf(SemanticFixture, file, marker);
        return Assert.Single(Output.Value.Definitions, d => d.File == file && d.StartLine == line);
    }

    /// <summary>The single non-definition reference on a Usage.cs call-site line.</summary>
    private static HelperHarness.Reference UsageAt(string marker)
    {
        var line = HelperHarness.LineOf(SemanticFixture, "Usage.cs", marker);
        return Assert.Single(
            Output.Value.References,
            r => r is { File: "Usage.cs", IsDefinition: false } && r.Line == line);
    }

    [Fact] // (a) overload selection: Add(int) vs Add(string), resolved per call site
    public void Overloads_ResolvePerCallSite()
    {
        var addInt = DefinitionAt("Calculator.cs", "int Add(int");
        var addString = DefinitionAt("Calculator.cs", "string Add(string");
        Assert.NotEqual(addInt.Docid, addString.Docid); // distinct identities, same name

        Assert.Equal(addInt.Docid, UsageAt("calc.Add(42)").Docid);
        Assert.Equal(addString.Docid, UsageAt("calc.Add(\"hello\")").Docid);
    }

    [Fact] // (b1) inherited member: circle.Describe() resolves to ShapeBase.Describe
    public void InheritedMember_ResolvesToDeclaringBaseClass()
    {
        var describe = DefinitionAt("Shapes.cs", "string Describe()");

        Assert.Equal(describe.Docid, UsageAt("circle.Describe()").Docid);
    }

    [Fact] // (b2) interface member: shape.Area() resolves to IShape.Area, not Circle.Area
    public void InterfaceMemberCall_ResolvesToInterfaceMember()
    {
        var interfaceArea = DefinitionAt("Shapes.cs", "double Area();");
        var circleArea = DefinitionAt("Shapes.cs", "double Area() =>");
        Assert.NotEqual(interfaceArea.Docid, circleArea.Docid);

        // UsageAt's Assert.Single also proves the call is not double-attributed to Circle.Area.
        Assert.Equal(interfaceArea.Docid, UsageAt("shape.Area()").Docid);
    }

    [Fact] // (c) generic instantiation: Repository<User>.Get resolves to the generic definition
    public void GenericInstantiation_ResolvesToGenericDefinition()
    {
        var get = DefinitionAt("Repository.cs", "T Get()");

        Assert.Equal(get.Docid, UsageAt("repo.Get()").Docid);
    }
}
