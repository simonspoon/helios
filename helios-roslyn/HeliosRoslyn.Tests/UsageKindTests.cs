using Xunit;

namespace HeliosRoslyn.Tests;

/// <summary>
/// Task 913: usage_kind on every reference -- read/write/readwrite/unknown, decided purely
/// from the syntax at the usage site (design.md). fixtures/usage-kind/Fixture.cs pairs a
/// ref-returning property and indexer with an ordinary auto-property, since only an
/// addressable (ref-returning) member can be targeted by out/ref/in or `ref` aliasing.
/// </summary>
public class UsageKindTests
{
    private static readonly string UsageKindFixture = HelperHarness.Fixture("usage-kind");

    // One analyze run shared by all cases.
    private static readonly Lazy<HelperHarness.Records> Output =
        new(() => HelperHarness.Analyze("--root", UsageKindFixture));

    /// <summary>The one non-definition reference on <paramref name="marker"/>'s line whose docid ends with <paramref name="docidSuffix"/>.</summary>
    private static HelperHarness.Reference ReferenceAt(string marker, string docidSuffix)
    {
        var line = HelperHarness.LineOf(UsageKindFixture, "Fixture.cs", marker);
        return Assert.Single(
            Output.Value.References,
            r => r is { File: "Fixture.cs", IsDefinition: false } && r.Line == line
                 && r.Docid.EndsWith(docidSuffix, StringComparison.Ordinal));
    }

    [Fact] // plain read
    public void PlainRead_IsClassifiedRead()
    {
        Assert.Equal("read", ReferenceAt("var read = counter.Value;", "Counter.Value").UsageKind);
    }

    [Fact] // simple assignment write
    public void SimpleAssignment_IsClassifiedWrite()
    {
        Assert.Equal("write", ReferenceAt("counter.Value = 1;", "Counter.Value").UsageKind);
    }

    [Fact] // compound assignment readwrite
    public void CompoundAssignment_IsClassifiedReadWrite()
    {
        Assert.Equal("readwrite", ReferenceAt("counter.Value += 1;", "Counter.Value").UsageKind);
    }

    [Fact] // ++ readwrite
    public void Increment_IsClassifiedReadWrite()
    {
        Assert.Equal("readwrite", ReferenceAt("counter.Value++;", "Counter.Value").UsageKind);
    }

    [Fact] // receiver-is-read in a nested member write: `counter.Inner.Value = 2` reads
           // Inner (just walking to get there) but writes Value
    public void NestedMemberWrite_ReceiverIsRead_TargetIsWrite()
    {
        Assert.Equal("read", ReferenceAt("counter.Inner.Value = 2;", "Counter.Inner").UsageKind);
        Assert.Equal("write", ReferenceAt("counter.Inner.Value = 2;", "Counter.Value").UsageKind);
    }

    [Fact] // plain field: simple assignment write
    public void FieldSimpleAssignment_IsClassifiedWrite()
    {
        Assert.Equal("write", ReferenceAt("counter.Count = 1;", "Counter.Count").UsageKind);
    }

    [Fact] // plain field: ++ readwrite
    public void FieldIncrement_IsClassifiedReadWrite()
    {
        Assert.Equal("readwrite", ReferenceAt("counter.Count++;", "Counter.Count").UsageKind);
    }

    [Fact] // a plain field is indexed with kind "field", not "fn"
    public void Field_IsEmittedWithFieldKind()
    {
        var definition = Assert.Single(Output.Value.Definitions, d => d.Docid == "F:UsageKind.Counter.Count");
        Assert.Equal("field", definition.Kind);
    }

    [Fact] // a property is indexed with kind "property", distinct from a plain field
    public void Property_IsEmittedWithPropertyKind()
    {
        var definition = Assert.Single(Output.Value.Definitions, d => d.Docid == "P:UsageKind.Counter.Value");
        Assert.Equal("property", definition.Kind);
    }

    [Fact] // indexer write
    public void IndexerAssignment_IsClassifiedWrite()
    {
        Assert.Equal("write", ReferenceAt("bag[3] = 9;", "Bag.Item(System.Int32)").UsageKind);
    }

    [Fact] // C# `out` write
    public void OutArgument_IsClassifiedWrite()
    {
        Assert.Equal("write", ReferenceAt("Modifiers.TakeOut(out bag.First);", "Bag.First").UsageKind);
    }

    [Fact] // C# `ref` readwrite
    public void RefArgument_IsClassifiedReadWrite()
    {
        Assert.Equal("readwrite", ReferenceAt("Modifiers.TakeRef(ref bag.First);", "Bag.First").UsageKind);
    }

    [Fact] // C# `in` read
    public void InArgument_IsClassifiedRead()
    {
        Assert.Equal("read", ReferenceAt("Modifiers.TakeIn(in bag.First);", "Bag.First").UsageKind);
    }

    [Fact] // a `ref` alias cannot be classified confidently -> unknown, never a guessed read
    public void RefAlias_IsClassifiedUnknown()
    {
        Assert.Equal("unknown", ReferenceAt("ref int aliased = ref bag[0];", "Bag.Item(System.Int32)").UsageKind);
    }

    [Fact] // a declaration site is not a usage -> unknown, never a guessed read
    public void DeclarationSite_IsClassifiedUnknown()
    {
        var line = HelperHarness.LineOf(UsageKindFixture, "Fixture.cs", "public int Value { get; set; }");
        var definition = Assert.Single(
            Output.Value.References,
            r => r is { File: "Fixture.cs", IsDefinition: true } && r.Line == line);

        Assert.Equal("unknown", definition.UsageKind);
    }

    [Fact] // flat tuple deconstruction: every element is a write, not the generic-argument read
    public void FlatTupleDeconstruction_IsClassifiedWrite()
    {
        Assert.Equal("write", ReferenceAt("(pair.A, pair.B) = TupleSource.GetPair();", "Pair.A").UsageKind);
        Assert.Equal("write", ReferenceAt("(pair.A, pair.B) = TupleSource.GetPair();", "Pair.B").UsageKind);
    }

    [Fact] // nested tuple deconstruction: the write climbs through every enclosing tuple level
    public void NestedTupleDeconstruction_IsClassifiedWrite()
    {
        const string marker = "(nested.A, (nested.B, nested.C)) = TupleSource.GetNestedPair();";
        Assert.Equal("write", ReferenceAt(marker, "Pair.A").UsageKind);
        Assert.Equal("write", ReferenceAt(marker, "Pair.B").UsageKind);
        Assert.Equal("write", ReferenceAt(marker, "Pair.C").UsageKind);
    }

    [Fact] // a tuple on the right of `=` (or anywhere else that isn't an assignment target) is
           // still a plain read, same as any other argument-position usage
    public void TupleOnRightOfAssignment_IsClassifiedRead()
    {
        const string marker = "var snapshot = (pair.A, pair.B);";
        Assert.Equal("read", ReferenceAt(marker, "Pair.A").UsageKind);
        Assert.Equal("read", ReferenceAt(marker, "Pair.B").UsageKind);
    }
}
