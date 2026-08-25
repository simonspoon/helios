namespace Semantic;

public class Calculator
{
    public int Add(int value) => value;

    public string Add(string value) => value;

    public int Zero
    {
        get { return Add(0); }
    }

    private readonly Calculator _self = new Calculator();
}
