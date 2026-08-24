using Microsoft.Maui.Controls;
using MauiApp.ViewModels;

namespace MauiApp.Views;

public partial class DetailPage : ContentPage
{
    public DetailPage()
    {
        InitializeComponent();
        // No x:DataType in DetailPage.xaml; this assignment is the only thing
        // that says what its bindings resolve against.
        BindingContext = new DetailViewModel();
    }
}
