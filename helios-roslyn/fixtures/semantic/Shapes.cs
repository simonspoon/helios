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
