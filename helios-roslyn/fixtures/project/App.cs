namespace ProjectFixture;

public class App
{
    public static string Run()
    {
        var widget = new Widget();
        return widget.Ping();
    }
}
