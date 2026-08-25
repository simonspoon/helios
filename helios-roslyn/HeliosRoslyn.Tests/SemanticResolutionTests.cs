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

    [Fact] // (d1) container_docid: a reference inside a method attributes to that method
    public void Reference_InsideMethod_ContainerIsEnclosingMethod()
    {
        var run = DefinitionAt("Usage.cs", "static void Run()");

        Assert.Equal(run.Docid, UsageAt("calc.Add(42)").ContainerDocid);
    }

    [Fact] // (d2) container_docid: file/namespace-scope reference has no enclosing symbol
    public void Reference_AtNamespaceScope_ContainerIsNull()
    {
        var namespaceDeclaration = Assert.Single(
            Output.Value.References,
            r => r is { File: "Usage.cs", IsDefinition: true } && r.Docid == "N:Semantic");

        Assert.Null(namespaceDeclaration.ContainerDocid);
    }

    [Fact] // (d3) container_docid: lambda and local-function bodies attribute to the enclosing member
    public void Reference_InsideLambdaOrLocalFunction_ContainerIsEnclosingMember()
    {
        var run = DefinitionAt("Usage.cs", "static void Run()");

        Assert.Equal(run.Docid, UsageAt("calc.Add(1)").ContainerDocid); // lambda body
        Assert.Equal(run.Docid, UsageAt("calc.Add(2)").ContainerDocid); // local function body
    }

    [Fact] // (d4) container_docid: a property accessor body attributes to the enclosing property,
           // not the get-accessor method (which pass 1 never emits as a definition)
    public void Reference_InsidePropertyAccessor_ContainerIsEnclosingProperty()
    {
        var zero = DefinitionAt("Calculator.cs", "int Zero");
        var line = HelperHarness.LineOf(SemanticFixture, "Calculator.cs", "return Add(0);");
        var reference = Assert.Single(
            Output.Value.References,
            r => r is { File: "Calculator.cs", IsDefinition: false } && r.Line == line);

        Assert.Equal(zero.Docid, reference.ContainerDocid);
    }

    [Fact] // (d5) container_docid: a field initializer expression attributes to the field itself
           // (fields are an indexed kind, so the field symbol is a usable container); the field's
           // own type annotation, coming before the field is "entered", still attributes to the
           // containing type.
    public void Reference_InsideFieldInitializer_ContainerIsField()
    {
        var calculator = DefinitionAt("Calculator.cs", "class Calculator");
        var self = DefinitionAt("Calculator.cs", "_self = new Calculator()");
        var line = HelperHarness.LineOf(SemanticFixture, "Calculator.cs", "_self = new Calculator();");
        var references = Output.Value.References
            .Where(r => r is { File: "Calculator.cs", IsDefinition: false } && r.Line == line)
            .OrderBy(r => r.Col)
            .ToList();

        // the field's declared type ("Calculator _self") and the initializer's `new Calculator()`
        var typeAnnotation = Assert.Single(references, r => r.Col < self.StartCol);
        var initializer = Assert.Single(references, r => r.Col > self.StartCol);

        Assert.Equal(calculator.Docid, typeAnnotation.ContainerDocid);
        Assert.Equal(self.Docid, initializer.ContainerDocid);
    }

    [Fact] // (e1) relation: a class extending a base and implementing an interface emits both edges
    public void Relation_ClassExtendsAndImplements_EmitsBothEdges()
    {
        var shapeBase = DefinitionAt("Shapes.cs", "class ShapeBase");
        var iShape = DefinitionAt("Shapes.cs", "interface IShape");
        var circle = DefinitionAt("Shapes.cs", "class Circle");

        var extends = Assert.Single(
            Output.Value.Relations, r => r.SubDocid == circle.Docid && r.Kind == "extends");
        Assert.Equal(shapeBase.Docid, extends.SuperDocid);
        Assert.Equal("Shapes.cs", extends.File);

        var implements = Assert.Single(
            Output.Value.Relations, r => r.SubDocid == circle.Docid && r.Kind == "implements");
        Assert.Equal(iShape.Docid, implements.SuperDocid);
        Assert.Equal("Shapes.cs", implements.File);
    }

    [Fact] // (e2) relation: System.Object is the implicit base every class gets — never a relation row
    public void Relation_ImplicitObjectBase_IsNotEmitted()
    {
        var shapeBase = DefinitionAt("Shapes.cs", "class ShapeBase");

        Assert.DoesNotContain(Output.Value.Relations, r => r.SubDocid == shapeBase.Docid && r.Kind == "extends");
    }

    [Fact] // (e3) relation: an external (BCL) base type still gets a row with a populated super_name,
           // whether or not helios indexed it well enough to resolve super_docid
    public void Relation_ExternalBaseType_HasPopulatedSuperName()
    {
        var shapeError = DefinitionAt("Shapes.cs", "class ShapeError");

        var extends = Assert.Single(
            Output.Value.Relations, r => r.SubDocid == shapeError.Docid && r.Kind == "extends");
        Assert.Equal("System.Exception", extends.SuperName);
        Assert.Equal("Shapes.cs", extends.File);
    }

    [Fact] // (e4) relation: a partial type's Interfaces set is the union across parts — exactly
           // one relation row per edge, never one per (edge, declaring part) combination
    public void Relation_PartialType_UnionsInterfacesAcrossParts_NoDuplicates()
    {
        var fooPart1 = DefinitionAt("PartialType.cs", "partial class Foo : IAlpha");
        var iAlpha = DefinitionAt("PartialType.cs", "interface IAlpha");
        var iBeta = DefinitionAt("PartialType.cs", "interface IBeta");

        // Both parts share one docid, so this captures the type's relations as a whole.
        var fooRelations = Output.Value.Relations.Where(r => r.SubDocid == fooPart1.Docid).ToList();
        Assert.Equal(2, fooRelations.Count);
        Assert.Contains(fooRelations, r => r.Kind == "implements" && r.SuperDocid == iAlpha.Docid);
        Assert.Contains(fooRelations, r => r.Kind == "implements" && r.SuperDocid == iBeta.Docid);
    }
}
