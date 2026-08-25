namespace Semantic;

public interface IAlpha
{
}

public interface IBeta
{
}

// Two parts of the same type, each declaring a different interface. Interfaces
// is the union across both, so exactly two relation rows are expected for
// Foo overall — never four (one per part x per interface).
public partial class Foo : IAlpha
{
}

public partial class Foo : IBeta
{
}
