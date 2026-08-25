namespace Semantic;

public interface IShape
{
    double Area();
}

public abstract class ShapeBase
{
    public string Describe() => "shape";
}

public class Circle : ShapeBase, IShape
{
    public double Area() => 3.14;
}

// External (BCL) base type: never indexed as a definition, so its relation row
// must still carry a populated super_name regardless of whether super_docid
// resolves.
public class ShapeError : System.Exception
{
}
