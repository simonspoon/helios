namespace UsageKind;

public class Counter
{
    public int Value { get; set; }
    public Counter Inner { get; set; }
    public int Count;
}

/// <summary>
/// A ref-returning indexer and property so out/ref/in and `ref`-alias usages have an
/// addressable member to target: an ordinary auto-property or field cannot be passed by
/// ref/out/in, or aliased with `ref`.
/// </summary>
public class Bag
{
    private readonly int[] _items = new int[4];

    public ref int this[int i] => ref _items[i];

    public ref int First => ref _items[0];
}

public static class Modifiers
{
    public static void TakeOut(out int value) => value = 1;
    public static void TakeRef(ref int value) => value += 1;
    public static void TakeIn(in int value) { }
}

/// <summary>Three writable properties so a flat and a nested tuple deconstruction can each
/// target a distinct member, with none of them repeated on the same source line.</summary>
public class Pair
{
    public int A { get; set; }
    public int B { get; set; }
    public int C { get; set; }
}

public static class TupleSource
{
    public static (int, int) GetPair() => (1, 2);
    public static (int, (int, int)) GetNestedPair() => (1, (2, 3));
}

public static class Usage
{
    public static void Run()
    {
        var counter = new Counter { Inner = new Counter() };
        var bag = new Bag();

        var read = counter.Value;              // plain read
        counter.Value = 1;                      // simple assignment write
        counter.Value += 1;                     // compound assignment readwrite
        counter.Value++;                        // increment readwrite

        counter.Inner.Value = 2;                // receiver (Inner) is read; Value is write

        counter.Count = 1;                      // plain field: simple assignment write
        counter.Count++;                        // plain field: increment readwrite

        bag[3] = 9;                             // indexer write

        Modifiers.TakeOut(out bag.First);       // out write
        Modifiers.TakeRef(ref bag.First);       // ref readwrite
        Modifiers.TakeIn(in bag.First);         // in read

        ref int aliased = ref bag[0];           // ref-alias: unknown

        var pair = new Pair();
        (pair.A, pair.B) = TupleSource.GetPair();                       // flat tuple deconstruction: both write

        var nested = new Pair();
        (nested.A, (nested.B, nested.C)) = TupleSource.GetNestedPair(); // nested tuple deconstruction: all write

        var snapshot = (pair.A, pair.B);                                // tuple on the right of `=` stays a read
    }
}
