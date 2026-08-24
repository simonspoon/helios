using Microsoft.Maui.Controls;
using LibProject.ViewModels;

namespace LibProject.Views;

public partial class LibPage : ContentPage
{
    public LibPage()
    {
        InitializeComponent();
        // No x:DataType in LibPage.xaml; this assignment is the only thing
        // that says what its bindings resolve against. LibPage's declaration
        // lives in Lib's compilation, not App's — the case that used to crash
        // when App's compilation resolved LibPage via a ProjectReference.
        BindingContext = new LibViewModel();
    }
}
