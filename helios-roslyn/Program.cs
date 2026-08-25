using System.Collections.Concurrent;
using System.Text;
using System.Text.Json;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;
using Microsoft.CodeAnalysis.MSBuild;
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
                    Console.Error.WriteLine("helios-roslyn: usage: helios-roslyn <ping | analyze --root <path> [--project <csproj> ...] [--files <list-file>]>");
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

    /// <summary>
    /// Wire-contract version of this helper, reported by `ping` so helios can
    /// refuse semantic mode against a helper too old for its own calls instead
    /// of discovering the mismatch at analyze time. Bump whenever the contract
    /// gains something helios may rely on (1: `analyze --files`).
    /// </summary>
    private const int ProtocolVersion = 1;

    private static void Ping(TextWriter stdout)
    {
        WriteRecord(stdout, new
        {
            type = "ping",
            available = true,
            protocol_version = ProtocolVersion,
            dotnet_version = Environment.Version.ToString(),
            roslyn_version = typeof(Compilation).Assembly.GetName().Version?.ToString() ?? "unknown",
        });
    }

    private static async Task<int> Analyze(string[] args, TextWriter stdout)
    {
        string? rootArg = null;
        string? filesArg = null;
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
                case "--files" when i + 1 < args.Length:
                    filesArg = args[++i];
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

        // The caller-supplied indexed-file list (one root-relative path per line):
        // the Rust host owns the indexed-file vocabulary (its gitignore-driven walk),
        // so it tells us exactly which files to report on. Without --files (direct
        // CLI use, tests) fall back to the bin/obj/dot-dir heuristic.
        HashSet<string>? indexedFiles = null;
        if (filesArg is not null)
        {
            indexedFiles = File.ReadLines(filesArg)
                .Where(l => l.Length > 0)
                .Select(l => l.Replace('\\', '/'))
                .ToHashSet(StringComparer.Ordinal);
        }

        var compilations = await LoadWorkspace(root, projects, stdout);

        // Two passes. Pass 1 emits every definition declared in an indexed file and
        // records the declaration sites. Pass 2 binds each indexed file once and
        // emits a reference per resolved node whose target is a pass-1 docid — work
        // scales with source under --root, not with symbol count × solution size
        // (the old per-symbol FindReferencesAsync was quadratic and unusable on
        // large multi-TFM workspaces). Dedupe sets span compilations: a multi-TFM
        // project surfaces the same file once per target framework, and walking
        // each TFM's tree keeps references inside `#if` regions.
        var emittedDefinitions = new HashSet<(string Docid, string File, int Line, int Col)>();
        var emittedReferences = new HashSet<(string Docid, string File, int Line, int Col)>();
        var emittedRelations = new HashSet<(string SubDocid, string SuperKey, string Kind)>();
        var declarationSites = new HashSet<(string Path, int Line, int Col)>();
        foreach (var compilation in compilations)
        {
            EmitDefinitions(compilation, root, stdout, indexedFiles, emittedDefinitions, declarationSites, emittedRelations);
        }
        var rootDocids = emittedDefinitions.Select(d => d.Docid).ToHashSet(StringComparer.Ordinal);
        // When a later compilation is provably bind-identical to one already
        // walked (same trees, same references), its pass-2 output would be a
        // byte-for-byte repeat that emittedReferences swallows anyway — skip
        // the semantic re-bind entirely.
        var equivalenceMemo = new Dictionary<(Compilation, Compilation), bool>();
        var bound = new List<Compilation>();
        foreach (var compilation in compilations)
        {
            if (bound.Any(prior => AreBindEquivalent(prior, compilation, root, indexedFiles, equivalenceMemo)))
            {
                continue;
            }
            bound.Add(compilation);
            EmitReferences(compilation, root, stdout, indexedFiles, rootDocids, declarationSites, emittedReferences);
        }

        // Pass 3: XAML bindings resolved against the same compilations, emitted
        // on the same docids. Runs once over all of them, not per compilation.
        Xaml.EmitBindings(compilations, root, stdout, indexedFiles, rootDocids, emittedReferences);

        return 0;
    }

    private static void EmitDefinitions(
        Compilation compilation,
        string root,
        TextWriter stdout,
        HashSet<string>? indexedFiles,
        HashSet<(string Docid, string File, int Line, int Col)> emittedDefinitions,
        HashSet<(string Path, int Line, int Col)> declarationSites,
        HashSet<(string SubDocid, string SuperKey, string Kind)> emittedRelations)
    {
        foreach (var symbol in CollectIndexedSymbols(compilation))
        {
            var docid = symbol.GetDocumentationCommentId();
            if (docid is null)
            {
                continue; // contract: records with a null DocId are not emitted
            }

            // The file the relation record (if any) below gets attributed to:
            // the first declaring location in an indexed file, set once and
            // left alone on every subsequent partial-declaration location.
            string? relationFile = null;

            foreach (var location in symbol.Locations.Where(l => l.IsInSource))
            {
                var span = location.GetLineSpan();
                var file = RelativePath(root, span.Path);
                if (!IsIndexedFile(file, indexedFiles))
                {
                    continue; // contract: only symbols declared in indexed files are emitted
                }
                relationFile ??= file;
                declarationSites.Add((span.Path, span.StartLinePosition.Line, span.StartLinePosition.Character));
                if (!emittedDefinitions.Add((docid, file, span.StartLinePosition.Line + 1, span.StartLinePosition.Character + 1)))
                {
                    continue;
                }
                WriteRecord(stdout, new
                {
                    type = "definition",
                    docid,
                    name = symbol.Name,
                    kind = KindOf(symbol),
                    file,
                    start_line = span.StartLinePosition.Line + 1,
                    start_col = span.StartLinePosition.Character + 1,
                    end_line = DeclarationEndLine(symbol, location),
                    visibility = VisibilityOf(symbol),
                    scope = ScopeOf(symbol),
                });

                // The declaration site itself, flagged so the consumer never inserts it.
                var declarationModel = compilation.GetSemanticModel(location.SourceTree!);
                var declarationContainer = ContainerDocidAt(declarationModel, location.SourceSpan.Start);
                WriteReference(stdout, docid, file, span, isDefinition: true, declarationContainer);
            }

            // Once per type, not once per declaring location: BaseType and
            // Interfaces are already the union across every part of a partial
            // type, so the declared supertype set is a property of the type as
            // a whole, not of any one part's syntax. Attributing it to the
            // type's first declaring location is the honest answer — walking
            // syntax to work out which part wrote which edge is more machinery
            // than the question is worth.
            if (relationFile is not null && symbol is INamedTypeSymbol namedType)
            {
                EmitTypeRelations(stdout, namedType, docid, relationFile, emittedRelations);
            }
        }
    }

    /// <summary>
    /// Declared supertype edges for <paramref name="type"/>: the written base class
    /// (kind "extends") and the directly-declared interfaces (kind "implements").
    /// Deliberately <see cref="INamedTypeSymbol.Interfaces"/>, not
    /// <c>AllInterfaces</c> — this table records declared edges; transitive
    /// closure is a query concern, not something to bake into the wire.
    /// </summary>
    private static void EmitTypeRelations(
        TextWriter stdout,
        INamedTypeSymbol type,
        string docid,
        string file,
        HashSet<(string SubDocid, string SuperKey, string Kind)> emittedRelations)
    {
        // Only a class has a base class that was actually written in source.
        // System.Object is the implicit default every class gets whether or not
        // a base is written, so it is never a relation. Structs, enums, delegates
        // and interfaces have their own implicit base (ValueType, Enum,
        // MulticastDelegate, none) that Roslyn reports via BaseType/none of that
        // is written either, so only TypeKind.Class is considered here.
        if (type.TypeKind == TypeKind.Class && type.BaseType is { SpecialType: not SpecialType.System_Object } baseType)
        {
            WriteRelation(stdout, docid, baseType, "extends", file, emittedRelations);
        }

        foreach (var iface in type.Interfaces)
        {
            WriteRelation(stdout, docid, iface, "implements", file, emittedRelations);
        }
    }

    /// <summary>One on-wire relation record for a single declared supertype edge.</summary>
    private static void WriteRelation(
        TextWriter stdout,
        string subDocid,
        INamedTypeSymbol super,
        string kind,
        string file,
        HashSet<(string SubDocid, string SuperKey, string Kind)> emittedRelations)
    {
        var superDocid = super.GetDocumentationCommentId();
        var superName = super.ToDisplayString();
        // Dedupe key falls back to the display name when the supertype has no
        // DocId (metadata helios never indexed) — same reasoning as emittedDefinitions,
        // guarding against a multi-TFM project walking the same file more than once.
        // No File component: one relation row per logical (sub, super, kind)
        // edge, not one per declaring location.
        if (!emittedRelations.Add((subDocid, superDocid ?? superName, kind)))
        {
            return;
        }
        WriteRecord(stdout, new
        {
            type = "relation",
            sub_docid = subDocid,
            super_docid = superDocid,
            super_name = superName,
            kind,
            file,
        });
    }

    private static void EmitReferences(
        Compilation compilation,
        string root,
        TextWriter stdout,
        HashSet<string>? indexedFiles,
        HashSet<string> rootDocids,
        HashSet<(string Path, int Line, int Col)> declarationSites,
        HashSet<(string Docid, string File, int Line, int Col)> emittedReferences)
    {
        foreach (var tree in compilation.SyntaxTrees)
        {
            if (tree.FilePath.Length == 0)
            {
                continue; // in-memory tree: never an indexed file
            }
            var file = RelativePath(root, tree.FilePath);
            if (!IsIndexedFile(file, indexedFiles))
            {
                continue; // also skips source-generator trees (pseudo paths are never indexed)
            }

            var model = compilation.GetSemanticModel(tree);
            // descendIntoTrivia keeps doc-comment cref references.
            foreach (var node in tree.GetRoot().DescendantNodes(descendIntoTrivia: true))
            {
                ISymbol? symbol;
                Location location;
                switch (node)
                {
                    case IdentifierNameSyntax { IsVar: true }:
                        continue; // `var` binds to the inferred type; not an explicit usage
                    case SimpleNameSyntax name:
                        symbol = ResolveReferencedSymbol(model, name);
                        location = name.GetLocation();
                        break;
                    // `class D() : B(5)` is NOT handled: its type name is already
                    // visited as a SimpleNameSyntax at the same position, and the
                    // old FindReferences path treated the base invocation as
                    // implicit — emitting the ctor here would double-report the site.
                    case ConstructorInitializerSyntax init: // `: this(...)` / `: base(...)`
                        symbol = NormalizeConstructor(model.GetSymbolInfo(init).Symbol);
                        location = init.ThisOrBaseKeyword.GetLocation();
                        break;
                    case ElementAccessExpressionSyntax element: // indexer usage `x[i]`
                        symbol = model.GetSymbolInfo(element).Symbol?.OriginalDefinition;
                        location = element.ArgumentList.OpenBracketToken.GetLocation();
                        break;
                    case ElementBindingExpressionSyntax binding: // conditional indexer usage `x?[i]`
                        symbol = model.GetSymbolInfo(binding).Symbol?.OriginalDefinition;
                        location = binding.ArgumentList.OpenBracketToken.GetLocation();
                        break;
                    default:
                        continue;
                }

                var docid = symbol?.GetDocumentationCommentId();
                if (docid is null || !rootDocids.Contains(docid))
                {
                    continue; // framework/NuGet targets and locals are not indexed
                }

                var span = location.GetLineSpan();
                if (declarationSites.Contains((span.Path, span.StartLinePosition.Line, span.StartLinePosition.Character)))
                {
                    continue; // declaration sites are emitted once, flagged is_definition:true
                }
                if (!emittedReferences.Add((docid, file, span.StartLinePosition.Line + 1, span.StartLinePosition.Character + 1)))
                {
                    continue;
                }
                var containerDocid = ContainerDocidAt(model, location.SourceSpan.Start);
                WriteReference(stdout, docid, file, span, isDefinition: false, containerDocid);
            }
        }
    }

    /// <summary>
    /// True when two compilations provably bind identically, so pass 2 over
    /// <paramref name="b"/> would re-emit exactly what <paramref name="a"/>
    /// already produced: identical syntax trees (text and parse-relevant
    /// options; path too for indexed trees, whose path lands in output records)
    /// and identical references (same assembly files; project references
    /// bind-equivalent in turn). Conservative — any mismatch, including
    /// ordering, means "not equivalent" and the caller re-binds. Per-tree
    /// checks (e.g. ContainsDirectives alone) are NOT safe: binding depends on
    /// the whole compilation, so a directive-free tree can still resolve
    /// differently under a different reference set.
    /// </summary>
    internal static bool AreBindEquivalent(
        Compilation a, Compilation b, string root, HashSet<string>? indexedFiles,
        Dictionary<(Compilation, Compilation), bool> memo)
    {
        if (ReferenceEquals(a, b))
        {
            return true;
        }
        if (memo.TryGetValue((a, b), out var known))
        {
            return known;
        }
        // Assembly name participates in binding via InternalsVisibleTo.
        var result = string.Equals(a.AssemblyName, b.AssemblyName, StringComparison.Ordinal)
            && TreesMatch(a, b, root, indexedFiles)
            && ReferencesMatch(a, b, root, indexedFiles, memo);
        memo[(a, b)] = result;
        return result;
    }

    private static bool TreesMatch(Compilation a, Compilation b, string root, HashSet<string>? indexedFiles)
    {
        var treesA = a.SyntaxTrees.ToList();
        var treesB = b.SyntaxTrees.ToList();
        if (treesA.Count != treesB.Count)
        {
            return false;
        }
        for (var i = 0; i < treesA.Count; i++)
        {
            var (ta, tb) = (treesA[i], treesB[i]);
            if (!ta.GetText().ContentEquals(tb.GetText()) || !ParseIdentical(ta, tb))
            {
                return false;
            }
            // Binding is path-blind, so generated trees (each context's own
            // obj/…/AssemblyInfo.cs) may differ in path as long as their text
            // matches. Indexed trees must also agree on path: it becomes the
            // `file` field of every record they produce.
            if (!string.Equals(ta.FilePath, tb.FilePath, StringComparison.Ordinal)
                && (IsIndexedFile(RelativePath(root, ta.FilePath), indexedFiles)
                    || IsIndexedFile(RelativePath(root, tb.FilePath), indexedFiles)))
            {
                return false;
            }
        }
        return true;
    }

    /// <summary>
    /// Same text parses to the same tree only when the parse options agree — or
    /// when the sole disagreement is preprocessor symbols (net8.0 defines NET8_0,
    /// net9.0 NET9_0, …) and the tree carries no directives for them to toggle.
    /// </summary>
    private static bool ParseIdentical(SyntaxTree a, SyntaxTree b)
    {
        if (a.Options is not CSharpParseOptions oa || b.Options is not CSharpParseOptions ob)
        {
            return false;
        }
        if (oa.LanguageVersion != ob.LanguageVersion
            || oa.DocumentationMode != ob.DocumentationMode
            || oa.Kind != ob.Kind)
        {
            return false;
        }
        if (oa.PreprocessorSymbolNames.ToHashSet(StringComparer.Ordinal)
                .SetEquals(ob.PreprocessorSymbolNames))
        {
            return true;
        }
        return !a.GetRoot().ContainsDirectives && !b.GetRoot().ContainsDirectives;
    }

    private static bool ReferencesMatch(
        Compilation a, Compilation b, string root, HashSet<string>? indexedFiles,
        Dictionary<(Compilation, Compilation), bool> memo)
    {
        var refsA = a.References.ToList();
        var refsB = b.References.ToList();
        if (refsA.Count != refsB.Count)
        {
            return false;
        }
        for (var i = 0; i < refsA.Count; i++)
        {
            var (ra, rb) = (refsA[i], refsB[i]);
            if (ra.Properties.Kind != rb.Properties.Kind
                || ra.Properties.EmbedInteropTypes != rb.Properties.EmbedInteropTypes
                || !ra.Properties.Aliases.SequenceEqual(rb.Properties.Aliases, StringComparer.Ordinal))
            {
                return false;
            }
            switch (ra, rb)
            {
                case (PortableExecutableReference pa, PortableExecutableReference pb)
                    when pa.FilePath is not null
                         && string.Equals(pa.FilePath, pb.FilePath, StringComparison.Ordinal):
                    continue;
                // A ProjectReference surfaces as one CompilationReference per
                // TFM context; bind-equivalent targets keep the pair equivalent.
                case (CompilationReference ca, CompilationReference cb)
                    when AreBindEquivalent(ca.Compilation, cb.Compilation, root, indexedFiles, memo):
                    continue;
                default:
                    return false;
            }
        }
        return true;
    }

    /// <summary>
    /// The symbol a name node refers to, normalized to the docid vocabulary pass 1
    /// emits: only exactly-resolved usages (no candidate/ambiguous bindings),
    /// constructed generics and reduced extension methods back to their original
    /// definitions, `new T()` to the explicit constructor when one exists and to
    /// the type when the constructor is the implicit default.
    /// </summary>
    private static ISymbol? ResolveReferencedSymbol(SemanticModel model, SimpleNameSyntax node)
    {
        // Climb to the full name this simple name completes (`new My.Ns.Foo()`,
        // `new global::Foo()`) so the creation check below sees creation.Type.
        SyntaxNode typeNode = node;
        while ((typeNode.Parent is QualifiedNameSyntax qualified && qualified.Right == typeNode)
               || (typeNode.Parent is AliasQualifiedNameSyntax aliased && aliased.Name == typeNode))
        {
            typeNode = typeNode.Parent;
        }
        if (typeNode.Parent is ObjectCreationExpressionSyntax creation && creation.Type == typeNode
            && model.GetSymbolInfo(creation).Symbol is IMethodSymbol { IsImplicitlyDeclared: false } ctor)
        {
            return ctor.OriginalDefinition;
        }

        var symbol = model.GetSymbolInfo(node).Symbol;
        if (symbol is null)
        {
            return null;
        }
        if (symbol is IMethodSymbol { ReducedFrom: { } reduced })
        {
            symbol = reduced;
        }
        return NormalizeConstructor(symbol);
    }

    /// <summary>
    /// Implicitly-declared default constructors have no emitted definition;
    /// credit the containing type (`new Person()`, `[MyAttr]`, `: base()`).
    /// </summary>
    private static ISymbol? NormalizeConstructor(ISymbol? symbol) => symbol switch
    {
        IMethodSymbol { MethodKind: MethodKind.Constructor, IsImplicitlyDeclared: true } ctor =>
            ctor.ContainingType.OriginalDefinition,
        _ => symbol?.OriginalDefinition,
    };

    /// <summary>
    /// Picks the load path: explicit --project args, else a discovered .sln, else all
    /// discovered .csproj files (all via MSBuildWorkspace), else AdhocWorkspace over loose .cs.
    /// </summary>
    private static async Task<List<Compilation>> LoadWorkspace(
        string root, List<string> projectArgs, TextWriter stdout)
    {
        var projectPaths = projectArgs
            .Select(p => Path.GetFullPath(Path.IsPathRooted(p) ? p : Path.Combine(root, p)))
            .ToList();

        string? solutionPath = null;
        if (projectPaths.Count == 0)
        {
            solutionPath = EnumerateFiles(root, "*.sln")
                .OrderBy(p => RelativePath(root, p).Count(c => c == '/'))
                .ThenBy(p => p, StringComparer.Ordinal)
                .FirstOrDefault();
            if (solutionPath is null)
            {
                projectPaths = EnumerateFiles(root, "*.csproj");
            }
        }

        if (solutionPath is null && projectPaths.Count == 0)
        {
            return await LoadAdhocWorkspace(root, EnumerateFiles(root, "*.cs"));
        }
        return await LoadMsBuildWorkspace(solutionPath, projectPaths, stdout);
    }

    private static async Task<List<Compilation>> LoadMsBuildWorkspace(
        string? solutionPath, List<string> projectPaths, TextWriter stdout)
    {
        // Load failures inside an otherwise-good run surface as warning records (arch §1.2);
        // queued because workspace events may fire off-thread while we own stdout.
        var failures = new ConcurrentQueue<string>();
        var workspace = MSBuildWorkspace.Create();
        workspace.WorkspaceFailed += (_, e) =>
        {
            if (e.Diagnostic.Kind == WorkspaceDiagnosticKind.Failure)
            {
                failures.Enqueue(e.Diagnostic.Message);
            }
        };

        if (solutionPath is not null)
        {
            await workspace.OpenSolutionAsync(solutionPath);
        }
        else
        {
            foreach (var path in projectPaths)
            {
                // Skip projects already pulled in transitively via ProjectReference.
                if (workspace.CurrentSolution.Projects.All(p => p.FilePath != path))
                {
                    await workspace.OpenProjectAsync(path);
                }
            }
        }

        while (failures.TryDequeue(out var message))
        {
            WriteRecord(stdout, new { type = "warning", message });
        }

        var compilations = new List<Compilation>();
        foreach (var project in workspace.CurrentSolution.Projects.Where(p => p.Language == LanguageNames.CSharp))
        {
            compilations.Add(await project.GetCompilationAsync()
                ?? throw new InvalidOperationException($"project produced no compilation: {project.Name}"));
        }
        return compilations;
    }

    internal static List<string> EnumerateFiles(string root, string pattern)
    {
        var options = new EnumerationOptions
        {
            RecurseSubdirectories = true,
            AttributesToSkip = FileAttributes.Hidden | FileAttributes.System,
        };
        return Directory.EnumerateFiles(root, pattern, options)
            .Where(p => !IsExcludedPath(RelativePath(root, p)))
            .OrderBy(p => p, StringComparer.Ordinal)
            .ToList();
    }

    private static async Task<List<Compilation>> LoadAdhocWorkspace(string root, List<string> csFiles)
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
        return [compilation];
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
                        if (IsIndexedDefinitionKind(ns) && ns.Locations.Any(l => l.IsInSource))
                        {
                            result.Add(ns);
                        }
                        Visit(ns);
                        break;
                    case INamedTypeSymbol type when IsIndexedDefinitionKind(type):
                        if (type.Locations.Any(l => l.IsInSource))
                        {
                            result.Add(type);
                        }
                        Visit(type);
                        break;
                    case IMethodSymbol or IPropertySymbol when IsIndexedDefinitionKind(member):
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

    internal static string RelativePath(string root, string path) =>
        Path.GetRelativePath(root, path).Replace('\\', '/');

    /// <summary>A repo-relative path that escapes the analysis root.</summary>
    private static bool IsOutsideRoot(string relativePath) =>
        relativePath == ".." || relativePath.StartsWith("../", StringComparison.Ordinal);

    /// <summary>
    /// Is this root-relative path one the index reports on? With --files, exactly
    /// the caller's list; otherwise everything under root minus IsExcludedPath.
    /// </summary>
    internal static bool IsIndexedFile(string relativePath, HashSet<string>? indexedFiles)
    {
        if (IsOutsideRoot(relativePath))
        {
            return false;
        }
        return indexedFiles?.Contains(relativePath) ?? !IsExcludedPath(relativePath);
    }

    /// <summary>
    /// Heuristic used only without --files: build output and dot-directories —
    /// generated code (e.g. MAUI XAML codegen under obj/) that indexing rarely wants.
    /// </summary>
    private static bool IsExcludedPath(string relativePath) =>
        relativePath.Split('/').Any(part => part is "bin" or "obj" || part.StartsWith('.'));

    /// <summary>One on-wire reference record; the single place that converts 0-based spans to 1-based columns.</summary>
    private static void WriteReference(TextWriter stdout, string docid, string file, FileLinePositionSpan span, bool isDefinition, string? containerDocid = null) =>
        WriteReference(stdout, docid, file, span.StartLinePosition.Line + 1, span.StartLinePosition.Character + 1, isDefinition, containerDocid);

    /// <summary>Same record from an already-1-based position (the XAML pass has no Roslyn span).</summary>
    internal static void WriteReference(TextWriter stdout, string docid, string file, int line, int col, bool isDefinition, string? containerDocid = null) =>
        WriteRecord(stdout, new
        {
            type = "reference",
            docid,
            file,
            line,
            col,
            is_definition = isDefinition,
            container_docid = containerDocid,
        });

    /// <summary>
    /// The DocumentationCommentId of the nearest enclosing symbol at <paramref name="position"/>
    /// that pass 1 (<see cref="CollectIndexedSymbols"/>) actually emits as a definition, so the
    /// consumer can always resolve it: walk <see cref="SemanticModel.GetEnclosingSymbol"/> outward
    /// via <see cref="NextContainer"/>, accepting only symbols <see cref="IsIndexedDefinitionKind"/>
    /// allows — everything else (lambdas, local functions, accessors, field/event symbols, the
    /// synthesized top-level-statements entry point, ...) is climbed past regardless of whether it
    /// happens to carry its own non-null DocumentationCommentId. Null at file/namespace scope, or
    /// when nothing encloses the position at all.
    /// </summary>
    private static string? ContainerDocidAt(SemanticModel model, int position)
    {
        for (var symbol = model.GetEnclosingSymbol(position); symbol is not null; symbol = NextContainer(symbol))
        {
            // Original definition: pass 1 stamps declared members, never constructed
            // instantiations (e.g. inside Repository<User>.Get, the enclosing symbol
            // is the constructed Get, not the generic definition CollectIndexedSymbols
            // visited) — the same normalization ResolveReferencedSymbol applies to targets.
            var original = symbol.OriginalDefinition;
            if (original.IsImplicitlyDeclared || !IsIndexedDefinitionKind(original))
            {
                continue;
            }
            var docid = original.GetDocumentationCommentId();
            if (docid is not null)
            {
                return docid;
            }
        }
        return null;
    }

    /// <summary>
    /// The next symbol out from <paramref name="symbol"/>. For a property or event accessor,
    /// that is the property/event itself (<see cref="IMethodSymbol.AssociatedSymbol"/>) — NOT
    /// <c>ContainingSymbol</c>, which skips straight past it to the declaring type, because
    /// accessors are members of the type, not of the property, in Roslyn's symbol tree.
    /// </summary>
    private static ISymbol? NextContainer(ISymbol symbol) =>
        symbol is IMethodSymbol { AssociatedSymbol: { } associated } ? associated : symbol.ContainingSymbol;

    /// <summary>
    /// The symbol kinds pass 1 emits as definitions: the single predicate <see cref="CollectIndexedSymbols"/>
    /// and <see cref="ContainerDocidAt"/> both gate on, so the two can never drift apart.
    /// </summary>
    private static bool IsIndexedDefinitionKind(ISymbol symbol) => symbol switch
    {
        INamespaceSymbol => true,
        INamedTypeSymbol { TypeKind: TypeKind.Class or TypeKind.Struct or TypeKind.Interface or TypeKind.Enum } => true,
        IMethodSymbol { MethodKind: MethodKind.Ordinary or MethodKind.Constructor } => true,
        IPropertySymbol => true,
        _ => false,
    };

    internal static void WriteRecord(TextWriter stdout, object record) =>
        stdout.WriteLine(JsonSerializer.Serialize(record, JsonOpts));
}
