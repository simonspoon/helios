using System.Text;
using System.Text.Json;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.FindSymbols;
using Microsoft.CodeAnalysis.Text;

namespace HeliosRoslyn;

/// <summary>
/// One-shot helper: `ping` and `analyze --root &lt;path&gt;`, NDJSON on stdout,
/// diagnostics on stderr, non-zero exit on fatal failure. See .scratch/arch.md §1.
/// </summary>
internal static class Program
{
    private static readonly JsonSerializerOptions JsonOpts = new()
    {
        Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
    };

    public static async Task<int> Main(string[] args)
    {
        // NDJSON framing: UTF-8, no BOM, "\n" terminated, stdout carries records only.
        using var stdout = new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false))
        {
            NewLine = "\n",
            AutoFlush = false,
        };

        try
        {
            switch (args.FirstOrDefault())
            {
                case "ping":
                    Ping(stdout);
                    stdout.Flush();
                    return 0;
                case "analyze":
                    var code = await Analyze(args.Skip(1).ToArray(), stdout);
                    stdout.Flush();
                    return code;
                default:
                    Console.Error.WriteLine("helios-roslyn: usage: helios-roslyn <ping | analyze --root <path> [--project <csproj> ...]>");
                    return 2;
            }
        }
        catch (Exception ex)
        {
            stdout.Flush();
            Console.Error.WriteLine($"helios-roslyn: fatal: {ex.Message}");
            return 1;
        }
    }

    private static void Ping(TextWriter stdout)
    {
        WriteRecord(stdout, new
        {
            type = "ping",
            available = true,
            dotnet_version = Environment.Version.ToString(),
            roslyn_version = typeof(Compilation).Assembly.GetName().Version?.ToString() ?? "unknown",
        });
    }

    private static async Task<int> Analyze(string[] args, TextWriter stdout)
    {
        string? rootArg = null;
        var projects = new List<string>(); // accepted for forward compat; used by the MSBuild path (P2b)
        for (var i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--root" when i + 1 < args.Length:
                    rootArg = args[++i];
                    break;
                case "--project" when i + 1 < args.Length:
                    projects.Add(args[++i]);
                    break;
                default:
                    Console.Error.WriteLine($"helios-roslyn: unknown or incomplete argument: {args[i]}");
                    return 2;
            }
        }

        if (rootArg is null)
        {
            Console.Error.WriteLine("helios-roslyn: analyze requires --root <path>");
            return 2;
        }

        var root = Path.GetFullPath(rootArg);
        if (!Directory.Exists(root))
        {
            Console.Error.WriteLine($"helios-roslyn: root is not a directory: {root}");
            return 1;
        }

        var csFiles = EnumerateCsFiles(root);
        var (compilation, solution) = await LoadAdhocWorkspace(root, csFiles);
        var symbols = CollectIndexedSymbols(compilation);

        foreach (var symbol in symbols)
        {
            var docid = symbol.GetDocumentationCommentId();
            if (docid is null)
            {
                continue; // contract: records with a null DocId are not emitted
            }

            var declarationSites = new HashSet<(string File, int Line, int Col)>();
            foreach (var location in symbol.Locations.Where(l => l.IsInSource))
            {
                var span = location.GetLineSpan();
                declarationSites.Add((span.Path, span.StartLinePosition.Line, span.StartLinePosition.Character));
                WriteRecord(stdout, new
                {
                    type = "definition",
                    docid,
                    name = symbol.Name,
                    kind = KindOf(symbol),
                    file = RelativePath(root, span.Path),
                    start_line = span.StartLinePosition.Line + 1,
                    start_col = span.StartLinePosition.Character + 1,
                    end_line = DeclarationEndLine(symbol, location),
                    visibility = VisibilityOf(symbol),
                    scope = ScopeOf(symbol),
                });

                // The declaration site itself, flagged so the consumer never inserts it.
                WriteRecord(stdout, new
                {
                    type = "reference",
                    docid,
                    file = RelativePath(root, span.Path),
                    line = span.StartLinePosition.Line + 1,
                    col = span.StartLinePosition.Character + 1,
                    is_definition = true,
                });
            }

            foreach (var referenced in await SymbolFinder.FindReferencesAsync(symbol, solution))
            {
                foreach (var refLoc in referenced.Locations)
                {
                    if (refLoc.IsImplicit || refLoc.IsCandidateLocation || !refLoc.Location.IsInSource)
                    {
                        continue; // only explicit, exactly-resolved usages
                    }

                    var span = refLoc.Location.GetLineSpan();
                    if (declarationSites.Contains((span.Path, span.StartLinePosition.Line, span.StartLinePosition.Character)))
                    {
                        continue; // declaration sites are emitted once, flagged is_definition:true
                    }
                    WriteRecord(stdout, new
                    {
                        type = "reference",
                        docid,
                        file = RelativePath(root, span.Path),
                        line = span.StartLinePosition.Line + 1,
                        col = span.StartLinePosition.Character + 1,
                        is_definition = false,
                    });
                }
            }
        }

        return 0;
    }

    private static List<string> EnumerateCsFiles(string root)
    {
        var options = new EnumerationOptions
        {
            RecurseSubdirectories = true,
            AttributesToSkip = FileAttributes.Hidden | FileAttributes.System,
        };
        return Directory.EnumerateFiles(root, "*.cs", options)
            .Where(p =>
            {
                var rel = RelativePath(root, p);
                var parts = rel.Split('/');
                return !parts.Any(d => d is "bin" or "obj" || d.StartsWith('.'));
            })
            .OrderBy(p => p, StringComparer.Ordinal)
            .ToList();
    }

    private static async Task<(Compilation Compilation, Solution Solution)> LoadAdhocWorkspace(string root, List<string> csFiles)
    {
        var workspace = new AdhocWorkspace();
        var projectId = ProjectId.CreateNewId();

        var documents = csFiles.Select(path => DocumentInfo.Create(
            DocumentId.CreateNewId(projectId),
            name: RelativePath(root, path),
            filePath: path,
            loader: TextLoader.From(TextAndVersion.Create(
                SourceText.From(File.ReadAllText(path), Encoding.UTF8),
                VersionStamp.Create(),
                path))));

        var projectInfo = ProjectInfo.Create(
            projectId,
            VersionStamp.Create(),
            name: "helios-adhoc",
            assemblyName: "helios-adhoc",
            language: LanguageNames.CSharp,
            compilationOptions: new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary),
            parseOptions: new CSharpParseOptions(LanguageVersion.Latest),
            documents: documents,
            metadataReferences: RuntimeReferences());

        workspace.AddProject(projectInfo);
        var solution = workspace.CurrentSolution;

        var compilation = await solution.GetProject(projectId)!.GetCompilationAsync()
            ?? throw new InvalidOperationException("workspace produced no compilation");
        return (compilation, solution);
    }

    /// <summary>Reference the running runtime's assemblies so fixture code type-checks.</summary>
    private static IReadOnlyList<MetadataReference> RuntimeReferences()
    {
        var tpa = (string?)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")
                  ?? throw new InvalidOperationException("TRUSTED_PLATFORM_ASSEMBLIES unavailable");
        return tpa.Split(Path.PathSeparator)
            .Where(p => p.EndsWith(".dll", StringComparison.OrdinalIgnoreCase))
            .Select(p => (MetadataReference)MetadataReference.CreateFromFile(p))
            .ToList();
    }

    /// <summary>Symbols of the indexed kinds (arch.md §1.2 kind table), declared in source.</summary>
    private static List<ISymbol> CollectIndexedSymbols(Compilation compilation)
    {
        var result = new List<ISymbol>();

        void Visit(INamespaceOrTypeSymbol container)
        {
            foreach (var member in container.GetMembers())
            {
                if (member.IsImplicitlyDeclared)
                {
                    continue;
                }

                switch (member)
                {
                    case INamespaceSymbol ns:
                        if (ns.Locations.Any(l => l.IsInSource))
                        {
                            result.Add(ns);
                        }
                        Visit(ns);
                        break;
                    case INamedTypeSymbol type when type.TypeKind is TypeKind.Class or TypeKind.Struct or TypeKind.Interface or TypeKind.Enum:
                        if (type.Locations.Any(l => l.IsInSource))
                        {
                            result.Add(type);
                        }
                        Visit(type);
                        break;
                    case IMethodSymbol method when method.MethodKind is MethodKind.Ordinary or MethodKind.Constructor:
                    case IPropertySymbol:
                        if (member.Locations.Any(l => l.IsInSource))
                        {
                            result.Add(member);
                        }
                        break;
                }
            }
        }

        Visit(compilation.Assembly.GlobalNamespace);
        return result;
    }

    private static string KindOf(ISymbol symbol) => symbol switch
    {
        INamespaceSymbol => "mod",
        INamedTypeSymbol { TypeKind: TypeKind.Class } => "class",
        INamedTypeSymbol { TypeKind: TypeKind.Struct } => "struct",
        INamedTypeSymbol { TypeKind: TypeKind.Interface } => "interface",
        INamedTypeSymbol { TypeKind: TypeKind.Enum } => "enum",
        _ => "fn", // methods, constructors, properties — the only other collected kinds
    };

    private static string VisibilityOf(ISymbol symbol) =>
        symbol is INamespaceSymbol || symbol.DeclaredAccessibility == Accessibility.Public
            ? "pub"
            : "private";

    /// <summary>Containing type name, else containing namespace, else null (csharp.rs::find_scope vocabulary).</summary>
    private static string? ScopeOf(ISymbol symbol)
    {
        if (symbol.ContainingType is { } type)
        {
            return type.Name;
        }
        if (symbol.ContainingNamespace is { IsGlobalNamespace: false } ns)
        {
            return ns.ToDisplayString();
        }
        return null;
    }

    /// <summary>End line (1-based) of the full declaration node owning the identifier at <paramref name="identifierLocation"/>.</summary>
    private static int DeclarationEndLine(ISymbol symbol, Location identifierLocation)
    {
        var declaration = symbol.DeclaringSyntaxReferences
            .FirstOrDefault(r => r.SyntaxTree == identifierLocation.SourceTree
                                 && r.Span.Contains(identifierLocation.SourceSpan.Start))
            ?? symbol.DeclaringSyntaxReferences
                .FirstOrDefault(r => r.SyntaxTree == identifierLocation.SourceTree);

        if (declaration is null)
        {
            return identifierLocation.GetLineSpan().EndLinePosition.Line + 1;
        }
        return declaration.SyntaxTree.GetLineSpan(declaration.Span).EndLinePosition.Line + 1;
    }

    private static string RelativePath(string root, string path) =>
        Path.GetRelativePath(root, path).Replace('\\', '/');

    private static void WriteRecord(TextWriter stdout, object record) =>
        stdout.WriteLine(JsonSerializer.Serialize(record, JsonOpts));
}
