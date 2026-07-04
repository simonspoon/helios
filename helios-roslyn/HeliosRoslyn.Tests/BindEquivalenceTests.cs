using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// The pass-2 dedupe predicate: a compilation may be skipped only when it
/// provably binds identically to one already walked. These pin the soundness
/// edges — directives, parse options, reference sets, project-reference
/// recursion — that decide skip vs re-bind.
/// </summary>
public class BindEquivalenceTests
{
    private static readonly MetadataReference Corlib =
        MetadataReference.CreateFromFile(typeof(object).Assembly.Location);

    private static CSharpCompilation Compile(
        string source,
        string[]? symbols = null,
        LanguageVersion langVersion = LanguageVersion.Latest,
        MetadataReference[]? references = null,
        string assemblyName = "fixture")
    {
        var options = new CSharpParseOptions(langVersion, preprocessorSymbols: symbols);
        var tree = CSharpSyntaxTree.ParseText(source, options, path: "/x/Fixture.cs");
        return CSharpCompilation.Create(
            assemblyName,
            [tree],
            references ?? [Corlib],
            new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary));
    }

    private static bool Equivalent(Compilation a, Compilation b) =>
        Program.AreBindEquivalent(a, b, "/x", indexedFiles: null, []);

    private static CSharpCompilation WithExtraTree(CSharpCompilation c, string source, string path) =>
        c.AddSyntaxTrees(CSharpSyntaxTree.ParseText(
            source, (CSharpParseOptions)c.SyntaxTrees[0].Options, path: path));

    [Fact] // the multi-TFM shape the skip exists for: same source, only the TFM symbols differ
    public void DirectiveFreeSource_DifferentSymbols_Equivalent()
    {
        var a = Compile("class C { }", symbols: ["NET8_0"]);
        var b = Compile("class C { }", symbols: ["NET9_0"]);
        Assert.True(Equivalent(a, b));
    }

    [Fact] // a directive can react to the differing symbols → parse may differ → re-bind
    public void DirectiveInSource_DifferentSymbols_NotEquivalent()
    {
        const string source = "#if NET8_0\nclass C { }\n#endif\n";
        var a = Compile(source, symbols: ["NET8_0"]);
        var b = Compile(source, symbols: ["NET9_0"]);
        Assert.False(Equivalent(a, b));
    }

    [Fact] // directives are harmless when the symbol sets agree
    public void DirectiveInSource_SameSymbols_Equivalent()
    {
        const string source = "#if NET8_0\nclass C { }\n#endif\n";
        var a = Compile(source, symbols: ["NET8_0"]);
        var b = Compile(source, symbols: ["NET8_0"]);
        Assert.True(Equivalent(a, b));
    }

    [Fact]
    public void DifferentText_NotEquivalent()
    {
        Assert.False(Equivalent(Compile("class C { }"), Compile("class D { }")));
    }

    [Fact] // language version changes how identical text parses
    public void DifferentLanguageVersion_NotEquivalent()
    {
        var a = Compile("class C { }", langVersion: LanguageVersion.CSharp11);
        var b = Compile("class C { }", langVersion: LanguageVersion.CSharp12);
        Assert.False(Equivalent(a, b));
    }

    [Fact] // different reference assemblies can flip overload/extension resolution
    public void DifferentReferences_NotEquivalent()
    {
        var extra = MetadataReference.CreateFromFile(typeof(Uri).Assembly.Location);
        var a = Compile("class C { }", references: [Corlib]);
        var b = Compile("class C { }", references: [Corlib, extra]);
        Assert.False(Equivalent(a, b));
    }

    [Fact] // InternalsVisibleTo keys on assembly name, so it participates in binding
    public void DifferentAssemblyName_NotEquivalent()
    {
        var a = Compile("class C { }", assemblyName: "one");
        var b = Compile("class C { }", assemblyName: "two");
        Assert.False(Equivalent(a, b));
    }

    [Fact] // each context's generated obj/…/AssemblyInfo.cs differs only in path; binding is path-blind
    public void GeneratedTree_PathDiffers_ContentSame_Equivalent()
    {
        var attr = "[assembly: System.Reflection.AssemblyTitleAttribute(\"fixture\")]";
        var a = WithExtraTree(Compile("class C { }"), attr, "/x/obj/Debug/net8.0/Gen.AssemblyInfo.cs");
        var b = WithExtraTree(Compile("class C { }"), attr, "/x/obj/Release/net8.0/Gen.AssemblyInfo.cs");
        Assert.True(Equivalent(a, b));
    }

    [Fact] // an indexed tree's path becomes the `file` field of its records → must agree
    public void IndexedTree_PathDiffers_ContentSame_NotEquivalent()
    {
        var a = WithExtraTree(Compile("class C { }"), "class D { }", "/x/One.cs");
        var b = WithExtraTree(Compile("class C { }"), "class D { }", "/x/Two.cs");
        Assert.False(Equivalent(a, b));
    }

    [Fact] // A(tfm1)→B(tfm1) vs A(tfm2)→B(tfm2): equivalent targets keep the pair equivalent
    public void ProjectReferences_EquivalentTargets_Equivalent()
    {
        var b1 = Compile("public class B { }", symbols: ["NET8_0"], assemblyName: "b");
        var b2 = Compile("public class B { }", symbols: ["NET9_0"], assemblyName: "b");
        var a1 = Compile("class A { B? F; }", symbols: ["NET8_0"],
            references: [Corlib, b1.ToMetadataReference()], assemblyName: "a");
        var a2 = Compile("class A { B? F; }", symbols: ["NET9_0"],
            references: [Corlib, b2.ToMetadataReference()], assemblyName: "a");
        Assert.True(Equivalent(a1, a2));
    }

    [Fact] // a referenced project that differs across TFMs forces a re-bind upstream too
    public void ProjectReferences_DivergentTargets_NotEquivalent()
    {
        var b1 = Compile("public class B { }", symbols: ["NET8_0"], assemblyName: "b");
        var b2 = Compile("#if NET9_0\npublic class B { public int N; }\n#else\npublic class B { }\n#endif\n",
            symbols: ["NET9_0"], assemblyName: "b");
        var a1 = Compile("class A { B? F; }", symbols: ["NET8_0"],
            references: [Corlib, b1.ToMetadataReference()], assemblyName: "a");
        var a2 = Compile("class A { B? F; }", symbols: ["NET9_0"],
            references: [Corlib, b2.ToMetadataReference()], assemblyName: "a");
        Assert.False(Equivalent(a1, a2));
    }
}
