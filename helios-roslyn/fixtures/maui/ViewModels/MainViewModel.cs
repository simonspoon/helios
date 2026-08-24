using System.Collections.ObjectModel;
using Microsoft.Maui.Controls;

namespace MauiApp.ViewModels;

public abstract class BaseViewModel
{
    // Bound from MainPage.xaml but declared on the base: a name match against
    // MainViewModel's own members would miss this.
    public bool IsBusy { get; set; }
}

public class MainViewModel : BaseViewModel
{
    public string Query { get; set; } = "";

    // Same member name as SearchResult.Title; the DataTemplate's x:DataType is
    // the only thing that tells the two bindings apart.
    public string Title { get; set; } = "";

    public ICommand? SearchCommand { get; set; }

    public Profile Profile { get; } = new();

    public ObservableCollection<SearchResult> Results { get; } = new();
}

public class Profile
{
    public string DisplayName { get; set; } = "";
}

public class SearchResult
{
    // Declared `init` only so the test can address this line apart from
    // MainViewModel.Title above; the binding resolution is unaffected.
    public string Title { get; init; } = "";
}
