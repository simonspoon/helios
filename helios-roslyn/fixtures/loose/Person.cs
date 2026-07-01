namespace MyApp;

public class Person
{
    public string Name { get; set; } = "";

    public void Greet()
    {
        System.Console.WriteLine("Hi " + Name);
    }
}
