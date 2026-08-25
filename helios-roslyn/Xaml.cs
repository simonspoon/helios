using System.Xml;
using System.Xml.Linq;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp.Syntax;

namespace HeliosRoslyn;

/// <summary>
/// Third pass: XAML data bindings as references into the C# symbol graph.
///
/// Roslyn has no XAML parser and the generated `obj/**.xaml.g.cs` carries only
/// x:Name fields — XamlC compiles `{Binding}` straight to IL — so the markup is
/// read here as XML. What the sidecar adds over an XML reader elsewhere is the
/// <see cref="Compilation"/>: `x:DataType` resolves to a real
/// <see cref="INamedTypeSymbol"/>, each path segment to a member, and the
/// emitted docid is the same <c>GetDocumentationCommentId</c> string the C#
/// passes emit — so a binding lands on the ViewModel property's existing index
/// entry, through inheritance and generated members alike.
/// </summary>
internal static class Xaml
{
    /// <summary>The `x` namespace, 2009 (MAUI/WinUI) and 2006 (WPF/Xamarin) spellings.</summary>
    private static readonly string[] XamlNamespaces =
    [
        "http://schemas.microsoft.com/winfx/2009/xaml",
        "http://schemas.microsoft.com/winfx/2006/xaml",
    ];

    private sealed record Context(
        IReadOnlyList<Compilation> Compilations,
        string File,
        TextWriter Stdout,
        HashSet<string> RootDocids,
        HashSet<(string Docid, string File, int Line, int Col)> Emitted);

    /// <summary>
    /// Emits one reference record per resolved binding-path segment across every
    /// .xaml file under <paramref name="root"/>.
    /// </summary>
    /// <remarks>
    /// Gated by the caller's indexed-file list exactly like the C# passes: the
    /// host's walk owns which paths the index covers, and it lists `.xaml`
    /// alongside `.cs`.
    /// </remarks>
    internal static void EmitBindings(
        IReadOnlyList<Compilation> compilations,
        string root,
        TextWriter stdout,
        HashSet<string>? indexedFiles,
        HashSet<string> rootDocids,
        HashSet<(string Docid, string File, int Line, int Col)> emittedReferences)
    {
        foreach (var path in Program.EnumerateFiles(root, "*.xaml"))
        {
            var file = Program.RelativePath(root, path);
            if (!Program.IsIndexedFile(file, indexedFiles))
            {
                continue;
            }

            XDocument document;
            try
            {
                document = XDocument.Load(path, LoadOptions.SetLineInfo);
            }
            catch (XmlException ex)
            {
                // Malformed markup degrades to "no bindings from this file",
                // never to a failed analyze.
                Program.WriteRecord(stdout, new { type = "warning", message = $"{file}: {ex.Message}" });
                continue;
            }

            if (document.Root is not { } element)
            {
                continue;
            }

            var context = new Context(compilations, file, stdout, rootDocids, emittedReferences);
            // No x:DataType on the root: fall back to what the code-behind
            // assigns to BindingContext.
            var dataType = DataTypeOf(element, compilations) ?? BindingContextOf(element, compilations);
            Walk(element, dataType, itemType: null, context);
        }
    }

    private static void Walk(XElement element, ITypeSymbol? dataType, ITypeSymbol? itemType, Context context)
    {
        // A DataTemplate without x:DataType binds against the item type of the
        // enclosing ItemsSource.
        var current = DataTypeOf(element, context.Compilations)
            ?? (element.Name.LocalName == "DataTemplate" ? itemType ?? dataType : dataType);

        foreach (var attribute in element.Attributes())
        {
            if (attribute.IsNamespaceDeclaration)
            {
                continue;
            }
            foreach (var (path, offset) in BindingPaths(attribute.Value))
            {
                EmitPath(path, offset, attribute, current, context);
            }
        }

        var childItemType = ItemTypeOf(element, current) ?? itemType;
        foreach (var child in element.Elements())
        {
            Walk(child, current, childItemType, context);
        }
    }

    /// <summary>Emits a reference for each path segment that resolves to an indexed member.</summary>
    private static void EmitPath(string path, int valueOffset, XAttribute attribute, ITypeSymbol? dataType, Context context)
    {
        if (dataType is null || attribute is not IXmlLineInfo info || !info.HasLineInfo())
        {
            return;
        }

        // LinePosition is the first character of the attribute name; the value
        // opens after `name="`. Attribute values holding an entity reference or
        // spanning lines would shift this — bindings do neither in practice.
        var valueColumn = info.LinePosition + WrittenNameLength(attribute) + 2;

        WalkPath(path, dataType, (symbol, segmentOffset) =>
        {
            var docid = symbol.GetDocumentationCommentId();
            if (docid is null || !context.RootDocids.Contains(docid))
            {
                return; // framework targets are not indexed
            }
            var column = valueColumn + valueOffset + segmentOffset;
            if (!context.Emitted.Add((docid, context.File, info.LineNumber, column)))
            {
                return;
            }
            // No C# enclosing symbol for a binding expression in markup.
            Program.WriteReference(context.Stdout, docid, context.File, info.LineNumber, column, isDefinition: false, containerDocid: null);
        });
    }

    /// <summary>
    /// Resolves each dot-separated segment of <paramref name="path"/> against
    /// <paramref name="type"/>, reporting every member it lands on, and returns
    /// the type the whole path evaluates to (null once a segment fails).
    /// </summary>
    private static ITypeSymbol? WalkPath(string path, ITypeSymbol? type, Action<ISymbol, int>? visit)
    {
        var offset = 0;
        foreach (var segment in path.Split('.'))
        {
            var start = offset + (segment.Length - segment.TrimStart().Length);
            offset += segment.Length + 1;
            if (type is null)
            {
                return null;
            }

            var bracket = segment.IndexOf('[');
            var name = (bracket < 0 ? segment : segment[..bracket]).Trim();
            if (name.Length > 0)
            {
                var member = LookupMember(type, name);
                if (member is null)
                {
                    return null;
                }
                visit?.Invoke(member, start);
                type = MemberType(member);
            }
            if (bracket >= 0 && type is not null)
            {
                type = ElementTypeOf(type) ?? type; // `Results[0]` steps into the item type
            }
        }
        return type;
    }

    /// <summary>Nearest declaration of <paramref name="name"/>, base classes then interfaces.</summary>
    private static ISymbol? LookupMember(ITypeSymbol type, string name)
    {
        for (ITypeSymbol? current = type; current is not null; current = current.BaseType)
        {
            if (Bindable(current, name) is { } member)
            {
                return member;
            }
        }
        foreach (var @interface in type.AllInterfaces)
        {
            if (Bindable(@interface, name) is { } member)
            {
                return member;
            }
        }
        return null;
    }

    private static ISymbol? Bindable(ITypeSymbol type, string name) =>
        type.GetMembers(name).FirstOrDefault(s => s is IPropertySymbol or IFieldSymbol)?.OriginalDefinition;

    private static ITypeSymbol? MemberType(ISymbol symbol) => symbol switch
    {
        IPropertySymbol property => property.Type,
        IFieldSymbol field => field.Type,
        _ => null,
    };

    /// <summary>T of an array or an IEnumerable&lt;T&gt; the type implements.</summary>
    private static ITypeSymbol? ElementTypeOf(ITypeSymbol type)
    {
        if (type is IArrayTypeSymbol array)
        {
            return array.ElementType;
        }
        var enumerable = type is INamedTypeSymbol named && IsEnumerable(named)
            ? named
            : type.AllInterfaces.FirstOrDefault(IsEnumerable);
        return enumerable?.TypeArguments.FirstOrDefault();
    }

    private static bool IsEnumerable(INamedTypeSymbol type) =>
        type.ConstructedFrom.SpecialType == SpecialType.System_Collections_Generic_IEnumerable_T;

    /// <summary>Item type implied by this element's `ItemsSource="{Binding …}"`, if any.</summary>
    private static ITypeSymbol? ItemTypeOf(XElement element, ITypeSymbol? dataType)
    {
        if (dataType is null || element.Attribute("ItemsSource") is not { } attribute)
        {
            return null;
        }
        var paths = BindingPaths(attribute.Value);
        if (paths.Count == 0)
        {
            return null;
        }
        var collection = WalkPath(paths[0].Path, dataType, visit: null);
        return collection is null ? null : ElementTypeOf(collection);
    }

    // --- Type names ---

    private static ITypeSymbol? DataTypeOf(XElement element, IReadOnlyList<Compilation> compilations) =>
        XamlAttribute(element, "DataType") is { } attribute
            ? ResolveXamlType(element, attribute.Value, compilations)
            : null;

    /// <summary>
    /// `vm:MainViewModel` in the scope of <paramref name="element"/>: the prefix
    /// is an ordinary XML namespace declaration, so XLinq's scoping resolves it
    /// and only the `clr-namespace:` payload needs unpacking.
    /// </summary>
    private static INamedTypeSymbol? ResolveXamlType(XElement element, string value, IReadOnlyList<Compilation> compilations)
    {
        value = value.Trim();
        var colon = value.IndexOf(':');
        var local = colon < 0 ? value : value[(colon + 1)..];
        var @namespace = colon < 0
            ? element.GetDefaultNamespace()
            : element.GetNamespaceOfPrefix(value[..colon]);
        if (@namespace is null || ClrNamespace(@namespace.NamespaceName) is not { } clr)
        {
            return null; // a MAUI/WinUI schema URL, not a CLR namespace
        }
        return ResolveType(compilations, clr.Length == 0 ? local : $"{clr}.{local}");
    }

    /// <summary>"clr-namespace:MauiApp.ViewModels;assembly=App" → "MauiApp.ViewModels".</summary>
    private static string? ClrNamespace(string namespaceName)
    {
        const string prefix = "clr-namespace:";
        if (!namespaceName.StartsWith(prefix, StringComparison.Ordinal))
        {
            return null;
        }
        var rest = namespaceName[prefix.Length..];
        var semicolon = rest.IndexOf(';');
        return semicolon < 0 ? rest : rest[..semicolon];
    }

    private static INamedTypeSymbol? ResolveType(IReadOnlyList<Compilation> compilations, string metadataName)
    {
        foreach (var compilation in compilations)
        {
            if (compilation.GetTypeByMetadataName(metadataName) is { } type)
            {
                return type;
            }
        }
        return null;
    }

    /// <summary>
    /// The type the code-behind assigns to BindingContext — the answer for pages
    /// written before compiled bindings, and the one thing an XML-only indexer
    /// cannot reach.
    /// </summary>
    private static ITypeSymbol? BindingContextOf(XElement root, IReadOnlyList<Compilation> compilations)
    {
        if (XamlAttribute(root, "Class") is not { } attribute)
        {
            return null;
        }
        foreach (var compilation in compilations)
        {
            if (compilation.GetTypeByMetadataName(attribute.Value.Trim()) is not { } codeBehind)
            {
                continue;
            }
            foreach (var reference in codeBehind.DeclaringSyntaxReferences)
            {
                var declaration = reference.GetSyntax();
                // GetTypeByMetadataName also resolves types pulled in via a ProjectReference;
                // a code-behind class declared in another project has a syntax tree that
                // `compilation` doesn't own, and GetSemanticModel throws on a tree it doesn't
                // own. Find whichever compilation actually owns the tree instead.
                var owner = compilation.ContainsSyntaxTree(declaration.SyntaxTree)
                    ? compilation
                    : compilations.FirstOrDefault(c => c.ContainsSyntaxTree(declaration.SyntaxTree));
                if (owner is null)
                {
                    continue;
                }
                var model = owner.GetSemanticModel(declaration.SyntaxTree);
                foreach (var assignment in declaration.DescendantNodes().OfType<AssignmentExpressionSyntax>())
                {
                    if (AssignedName(assignment.Left) != "BindingContext")
                    {
                        continue;
                    }
                    if (model.GetTypeInfo(assignment.Right).Type is { } assigned)
                    {
                        return assigned;
                    }
                }
            }
        }
        return null;
    }

    private static string? AssignedName(ExpressionSyntax expression) => expression switch
    {
        IdentifierNameSyntax identifier => identifier.Identifier.ValueText,
        MemberAccessExpressionSyntax member => member.Name.Identifier.ValueText,
        _ => null,
    };

    private static XAttribute? XamlAttribute(XElement element, string localName) =>
        XamlNamespaces
            .Select(ns => element.Attribute(XName.Get(localName, ns)))
            .FirstOrDefault(attribute => attribute is not null);

    /// <summary>Length of the attribute name as written in the file, prefix included.</summary>
    private static int WrittenNameLength(XAttribute attribute)
    {
        var local = attribute.Name.LocalName.Length;
        var prefix = attribute.Parent?.GetPrefixOfNamespace(attribute.Name.Namespace);
        return string.IsNullOrEmpty(prefix) ? local : prefix.Length + 1 + local;
    }

    // --- Markup extensions ---

    /// <summary>
    /// The Path of every `{Binding}` in an attribute value, with the offset of
    /// the path text within that value. Nested extensions are searched too, so
    /// `{Binding A, Converter={StaticResource C}}` and a CommandParameter
    /// binding both surface.
    /// </summary>
    internal static List<(string Path, int Offset)> BindingPaths(string value)
    {
        var paths = new List<(string, int)>();
        var start = 0;
        while (start < value.Length && char.IsWhiteSpace(value[start]))
        {
            start++;
        }
        // A value is either literal text or one extension; `{}` escapes a
        // literal brace and is never an extension.
        if (start >= value.Length || value[start] != '{' || value.AsSpan(start).StartsWith("{}"))
        {
            return paths;
        }
        ParseExtension(value, start, paths);
        return paths;
    }

    private static void ParseExtension(string value, int start, List<(string, int)> paths)
    {
        var i = start + 1;
        var nameStart = i;
        while (i < value.Length && value[i] != ' ' && value[i] != '}')
        {
            i++;
        }
        var isBinding = value[nameStart..i] is "Binding" or "x:Bind";

        var positional = 0;
        while (i < value.Length && value[i] != '}')
        {
            while (i < value.Length && (value[i] == ' ' || value[i] == ','))
            {
                i++;
            }
            if (i >= value.Length || value[i] == '}')
            {
                break;
            }

            // One member: `Name=Value` or a positional `Value`. Braces and
            // quotes nest, so only a top-level `,` or `}` ends it.
            var memberStart = i;
            var equals = -1;
            var depth = 0;
            var quote = '\0';
            while (i < value.Length)
            {
                var c = value[i];
                if (quote != '\0')
                {
                    if (c == quote)
                    {
                        quote = '\0';
                    }
                }
                else if (c is '\'' or '"')
                {
                    quote = c;
                }
                else if (c == '{')
                {
                    depth++;
                }
                else if (c == '}')
                {
                    if (depth == 0)
                    {
                        break;
                    }
                    depth--;
                }
                else if (depth == 0 && c == ',')
                {
                    break;
                }
                else if (depth == 0 && c == '=' && equals < 0)
                {
                    equals = i;
                }
                i++;
            }

            string? argument = null;
            var valueStart = memberStart;
            if (equals >= 0)
            {
                argument = value[memberStart..equals].Trim();
                valueStart = equals + 1;
            }
            var valueEnd = i;
            while (valueStart < valueEnd && char.IsWhiteSpace(value[valueStart]))
            {
                valueStart++;
            }
            while (valueEnd > valueStart && char.IsWhiteSpace(value[valueEnd - 1]))
            {
                valueEnd--;
            }

            if (valueStart < valueEnd && value[valueStart] == '{')
            {
                ParseExtension(value, valueStart, paths);
            }
            else if (isBinding && (argument == "Path" || (argument is null && positional == 0)))
            {
                if (valueStart < valueEnd && value[valueStart] is '\'' or '"' && value[valueEnd - 1] == value[valueStart])
                {
                    valueStart++;
                    valueEnd--;
                }
                if (valueStart < valueEnd)
                {
                    paths.Add((value[valueStart..valueEnd], valueStart));
                }
            }

            if (argument is null)
            {
                positional++;
            }
        }
    }
}
