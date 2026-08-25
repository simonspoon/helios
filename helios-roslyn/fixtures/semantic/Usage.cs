using System;

namespace Semantic;

public static class Usage
{
    public static void Run()
    {
        var calc = new Calculator();
        calc.Add(42);
        calc.Add("hello");

        var circle = new Circle();
        circle.Describe();
        IShape shape = circle;
        shape.Area();

        var repo = new Repository<User>();
        repo.Get();

        Action lambda = () => calc.Add(1);
        lambda();

        LocalAdd();
        return;

        void LocalAdd() => calc.Add(2);
    }
}
