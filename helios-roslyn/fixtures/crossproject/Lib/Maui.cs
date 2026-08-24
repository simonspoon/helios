namespace Microsoft.Maui.Controls;

// Stand-ins for the MAUI types the fixture views derive from; see fixtures/maui/Maui.cs.
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
