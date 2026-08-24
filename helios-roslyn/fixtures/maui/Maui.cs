namespace Microsoft.Maui.Controls;

// Stand-ins for the MAUI types the fixture views derive from. The XAML pass
// never binds against these — it only needs the code-behind classes to compile
// — so the fixture stays a plain net8.0 project with no MAUI workload.
public class BindableObject
{
    public object? BindingContext { get; set; }
}

public class ContentPage : BindableObject
{
    protected void InitializeComponent()
    {
    }
}

public interface ICommand
{
    void Execute(object? parameter);
}
