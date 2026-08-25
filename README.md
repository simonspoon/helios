<p align="center">
  <img src="icon.png" width="128" height="128" alt="helios">
</p>

# helios

Tree-sitter code indexing CLI with SQLite. Built for agent-driven codebase exploration.

## Overview

Helios parses source code with tree-sitter, extracts symbols, imports, and references, and stores them in a SQLite database (`.helios/index.db`). It supports incremental updates via git, multi-language indexing, and flexible querying.

### Supported Languages

| Language | Extensions |
|------------|------------------|
| Rust | `.rs` |
| Go | `.go` |
| Python | `.py` |
| TypeScript | `.ts`, `.tsx` |
| JavaScript | `.js`, `.jsx` |
| Swift | `.swift` |
| C# | `.cs` |
| XAML | `.xaml` |

XAML is indexed for its data bindings only — it declares no symbols of its own,
and its `{Binding}` paths are attributed to the C# members they name by the
optional Roslyn helper below.

## Installation

### Homebrew

```bash
brew install simonspoon/tap/helios
```

### From GitHub Releases

Download a pre-built binary from [Releases](https://github.com/simonspoon/helios/releases) and place it on your PATH.

### From Source

```bash
cargo install --git https://github.com/simonspoon/helios.git
```

## Semantic C# mode (optional)

By default helios resolves C# references syntactically with tree-sitter. An optional
Roslyn-based helper (`helios-roslyn`) upgrades `.cs` reference resolution to full
compiler-level accuracy: overloads, inheritance, interfaces, generics, and inferred
types resolve to the exact symbol instead of a name match.

The helper also resolves XAML data bindings, which nothing else in helios can:
a `{Binding Query}` in a `.xaml` file becomes a reference to the ViewModel
property it names, so `helios deps Query` lists the markup that binds it
alongside the C# that calls it. The binding context comes from `x:DataType`,
from the item type of an enclosing `ItemsSource` inside a `DataTemplate`, or
from whatever the code-behind assigns to `BindingContext`. Bindings whose
context is only known at runtime (dependency injection, Shell routes, a context
set by a parent view) are left unresolved rather than guessed.

Helios works fully without the helper — when it is absent, `helios init` silently
uses the tree-sitter path and `.xaml` files are indexed without bindings.
Nothing else changes.

### Requirements

- .NET SDK 8.0 or later on your PATH (`dotnet --version`). Projects with a
  `.csproj`/`.sln` need the SDK; repos of loose `.cs` files need only the runtime.

### Install the helper

With Homebrew (links the dll next to the brewed `helios` binary):

```bash
brew install simonspoon/tap/helios-csharp
```

Or download `helios-roslyn.zip` from [Releases](https://github.com/simonspoon/helios/releases)
and extract it next to the `helios` binary:

```bash
unzip helios-roslyn.zip -d "$(dirname "$(which helios)")"
```

Or place it anywhere and point `HELIOS_ROSLYN` at the dll (takes precedence):

```bash
export HELIOS_ROSLYN=/path/to/helios-roslyn.dll
```

Building from source instead: `dotnet publish helios-roslyn -c Release -o <dir>`,
then set `HELIOS_ROSLYN=<dir>/helios-roslyn.dll`.

### Verify

Run `helios init` in a C# project; the summary names the resolver, and so does
`helios status` afterwards:

```bash
helios init
# ...
# C# resolver: roslyn
```

`C# resolver: treesitter` means the helper wasn't used — a `warning:` line during
`init` says why (dotnet missing, runtime below .NET 8, helper too old, helper
failed). A missing helper with no `HELIOS_ROSLYN` set falls back silently by
design.

The helper ships with helios and reports a contract version to `helios init`; a
helper older than the helios binary expects is refused up front rather than
failing mid-analyze. Upgrade both together:

```bash
brew upgrade helios helios-csharp
```

## Usage

### `helios init`

Create a full index of the project. Walks all files (respecting `.gitignore`), parses symbols with tree-sitter, and stores results in `.helios/index.db`.

```bash
helios init
# Indexed 42 files (156 symbols) in 2.34s
# Database: /path/to/.helios/index.db
```

Options:
- `--timeout <SECONDS>` — Roslyn analyze timeout (default: 120). Large C# repos can exceed the default because the helper compiles sources first; also settable via the `HELIOS_ANALYZE_TIMEOUT` env var.

### `helios update`

Incrementally update the index. Uses `git diff` to detect changed files since the last indexed commit — only re-parses what changed.

```bash
helios update
# Updated: 3 files indexed, 0 deleted (12 symbols, 4 imports) in 0.23s
```

Falls back to a full re-index if not in a git repo or no previous commit is stored.

### `helios symbols [OPTIONS]`

List and filter indexed symbols.

```bash
# All functions
helios symbols --kind fn

# Symbols in a directory
helios symbols --file src/

# Search by name (regex)
helios symbols --grep "^parse_"

# Combine filters and paginate
helios symbols --kind struct --file src/parsers/ --limit 20 --offset 40
```

Options:
- `--file <PATH>` — Filter by file path (substring match)
- `--kind <KIND>` — Filter by symbol kind: `fn`, `struct`, `trait`, `enum`, `class`, `interface`, `type`, `const`, `mod`
- `--grep <PATTERN>` — Filter by symbol name (regex)
- `--scope <SCOPE>` — Filter by scope (e.g. impl block or class name)
- `--visibility <pub|private>` — Filter by visibility
- `--param <SUBSTR>` — Only symbols with a parameter whose source spelling contains this
- `--returns <SUBSTR>` — Only symbols whose return or declared type contains this
- `--body` — Include each symbol's source body in the output
- `--limit <N>` — Maximum number of symbols to return
- `--offset <N>` — Number of symbols to skip

Output format:

```
src/main.rs:42:0 fn pub main
src/lib.rs:10:4 struct pub Parser
src/Reporting.cs:6:18 fn pub Reporting.Compute(cfg: &Config) -> Result<(), Error>
```

Names are qualified with their scope (class, impl block, or namespace) when the
symbol has one, so same-named symbols stay distinguishable. When a symbol's
signature is known, it's appended: `(params)` for a callable, and
` -> returns` (callable) or `: returns` (field/const/variable) for its type.

### `helios deps <TARGET> [--scope <S>] [--file <P>] [--depth <N>]`

Show dependencies and dependents for a symbol or file. Auto-detects the target
type: a target with a `/` or a source-file extension is a file, anything else is
a symbol.

```bash
# File dependencies and dependents, both keyed by the file's own path
helios deps "src/util/money.ts"
# Dependencies (what src/util/money.ts imports):
#   -> std::collections (depth 1)
# Dependents (what imports src/util/money.ts):
#   -> src/domain/cart.ts (depth 1)
#   -> src/app.ts (depth 1)

# Transitive file dependencies (depth 3)
helios deps "src/parser.rs" --depth 3

# Symbol references
helios deps "parse_token"

# One definition of an ambiguous name, three ways to say it
helios deps "formatMoney" --file src/util
helios deps "Compute" --scope PromoPricing
helios deps "PromoPricing.Compute"
helios deps "src/util/money.ts:formatMoney"
```

- `--scope <S>` — Restrict a symbol target to definitions in this scope (class or impl block), matched exactly
- `--file <P>` — Restrict a symbol target to definitions in files matching this path (substring)
- `--depth <N>` — Transitive traversal depth (default: 1, file targets only)

A symbol name declared in more than one place is ambiguous, and an unnarrowed
target covers every definition of it. `--scope` and `--file` — or the equivalent
qualified spellings `Class.Method` and `path/to/file.ts:name` — select the
definitions you meant, and the query runs against only those. Symbol-mode JSON
carries a `definitions` array (path, line, scope) so the selection is visible.
A dotted target that names no definition is retried as a file, so a module path
such as `pkg.util.money` still works.

The resolved imports also decide which definition a *usage* belongs to: when a
file imports `formatMoney` from a specifier that resolves to
`src/legacy/money.ts`, its bare `formatMoney()` calls are listed against that
definition only, so `--file src/legacy/money.ts` prints the legacy callers
rather than every caller of the name. The narrowing is deliberately
conservative — a qualified usage (`wallet.formatMoney()`, `legacy.formatMoney()`),
a file that does not import the name, an aliased or unresolved import, and an
import of a file that does not define the name all keep the all-definitions
answer instead of guessing. C# under the Roslyn helper resolves each usage
exactly and is unaffected.

Import specifiers are resolved to indexed files at index time, so both
directions answer from the real file path however each importer spelled the
specifier (`./money`, `../util/money`, `crate::util::money`), and `--depth`
traverses file to file. Resolution covers TypeScript/JavaScript (relative
specifiers), Python (relative and dotted-absolute modules) and Rust
(`crate::` / `self::` / `super::` paths). Go, Swift and C# specifiers name a
package or namespace rather than a file: they are reported as raw specifier
text, and a raw specifier still works as a `deps` target.

### `helios flow <TARGET> [--scope <S>] [--file <P>] [--line <N>] [--mermaid]`

Show the control-flow graph of a single function or method body: branches,
loops, match arms, outward calls, and returns. Calls are leaf nodes labelled
with the callee — the callee's body is never expanded, so the graph stays
inside the one function you asked about.

**Rust and C# for now.** A target in any other indexed language exits with an
error naming the language; the graph builder is per-language and only those two
are mapped so far. A C# target is a method, a constructor, or an arrow-bodied
property; a property with `get`/`set` accessors is several bodies rather than
one, and is rejected as such.

```bash
# Bare name
helios flow parse_target

# A C# method, and one written as an expression body
helios flow "Orders.Total"
helios flow "Orders.Rate"

# One definition of an ambiguous name, three ways to say it
helios flow "run" --file src/commands/status.rs
helios flow "find_definitions" --scope Database
helios flow "Database.find_definitions"
helios flow "src/commands/deps.rs:parse_target"

# Two C# overloads share a name, a scope and a file; the line tells them apart
helios flow "Calc.Add" --line 6
```

- `--scope <S>` — Restrict the target to definitions in this scope (class or impl block), matched exactly
- `--file <P>` — Restrict the target to definitions in files matching this path (substring)
- `--line <N>` — Select the definition declared on this line
- `--mermaid` — Emit a mermaid flowchart instead of the indented tree

Target spellings match `helios deps`: a bare name, `Class.Method`, or
`path/to/file.rs:name`, plus `--scope`, `--file` and `--line` narrowing. A name
that matches more than one definition is an error listing the candidates with
their lines. C# overloads are what `--line` is for: they share a name, a scope
and a file, so the line is the only thing that separates them.

Default output is an indented tree. Nodes are numbered and carry a kind
(`entry`, `exit`, `branch`, `match`, `loop`, `call`, `return`, `break`,
`continue`, and in C# also `throw`, `yield` and `goto`) and a source line.
Bracketed labels are edges — `[true]`/`[false]` for branches, the pattern for
match arms and switch cases, `[body]`/`[repeat]`/`[done]` for loops, `[try]`
and `[catch …]` for a `try` statement, and `[no match]` for the path round a C#
`switch` that has no `default` label and so can match nothing. A `->` line is a
jump back to an already-printed node instead of repeating its subtree, and
`! Err ?` marks the early exit of a Rust `?` operator:

```
$ helios flow mermaid_shape
src/commands/flow.rs:18-29 mermaid_shape
#0 entry mermaid_shape(kind: &str, label: &str) -> String :18
#2 call escape_mermaid(…) :19
#3 match match kind :20
  ["entry" | "exit"]
    #4 call format! :21
    #5 return format!("([\"{text}\"])") :21
    #1 exit end :29
  ["branch" | "match"]
    #6 call format! :22
    #7 return format!("{{\"{text}\"}}") :22
    -> #1 exit (end)
  ["loop"]
    #8 call format! :23
    #9 return format!("{{{{\"{text}\"}}}}") :23
    -> #1 exit (end)
  ["return" | "throw"]
    #10 call format! :26
    #11 return format!("[/\"{text}\"/]") :26
    -> #1 exit (end)
  [_]
    #12 call format! :27
    #13 return format!("[\"{text}\"]") :27
    -> #1 exit (end)
```

`--mermaid` emits the same graph as a mermaid flowchart, ready to paste into
any diagram viewer:

```
$ helios flow mermaid_shape --mermaid
flowchart TD
  n0(["mermaid_shape(kind: &str, label: &str) -> String"])
  n1(["end"])
  n2["escape_mermaid(…)"]
  n3{"match kind"}
  n4["format!"]
  n5[/"format!(#quot;([\#quot;{text}\#quot;])#quot;)"/]
  n6["format!"]
  n7[/"format!(#quot;{{\#quot;{text}\#quot;}}#quot;)"/]
  n8["format!"]
  n9[/"format!(#quot;{{{{\#quot;{text}\#quot;}}}}#quot;)"/]
  n10["format!"]
  n11[/"format!(#quot;[/\#quot;{text}\#quot;/]#quot;)"/]
  n12["format!"]
  n13[/"format!(#quot;[\#quot;{text}\#quot;]#quot;)"/]
  n0 --> n2
  n2 --> n3
  n3 -->|"#quot;entry#quot; #124; #quot;exit#quot;"| n4
  n4 --> n5
  n5 --> n1
  n3 -->|"#quot;branch#quot; #124; #quot;match#quot;"| n6
  n6 --> n7
  n7 --> n1
  n3 -->|"#quot;loop#quot;"| n8
  n8 --> n9
  n9 --> n1
  n3 -->|"#quot;return#quot; #124; #quot;throw#quot;"| n10
  n10 --> n11
  n11 --> n1
  n3 -->|"_"| n12
  n12 --> n13
  n13 --> n1
```

Quotes and pipes inside a label are emitted as mermaid entity codes
(`#quot;`, `#124;`) so they render as text instead of closing the label early.

`--json` returns the graph as data: `function` (name, scope, file, line,
end_line, language, params, returns), `nodes` (`id`, `kind`, `label`, `line`)
and `edges` (`from`, `to`, and `label` where the edge is conditional).

Two deliberate omissions: calls inside a closure or a C# lambda are not
collected — they run when it is invoked, not at the point it is defined — and
nested `fn` items and C# local functions are not descended into. A statement
that makes no calls and takes no branch adds no node, so the graph shows control
flow rather than every line.

Three approximations, all in the same direction — a decision the graph draws as
a straight line. A branch nested inside an expression (`foo(if a { p() } else
{ q() })`) contributes `p` and `q` as sequential calls rather than as arms; a
`while` condition's calls sit before the loop header, so the back-edge re-enters
past them; and match-arm guards hang off the match node in parallel, so the
graph does not show that the second arm's guard only runs once the first fails.

C# adds five of its own, in the same direction. A ternary `?:` and the `??` and
`?.` short-circuits are drawn as a straight line rather than as a fork; a
`throw` inside a `try` goes to the function exit rather than to the enclosing
`catch`, and every `catch` edge is drawn from the entry of the `try`, so the
graph does not say which statement threw; a `do` loop is drawn with its test
first, so the graph does not show that the body always runs once; and a `switch`
*expression* that matches nothing throws at runtime, which the graph does not
draw — unlike a `switch` statement, whose missed path is the `[no match]` edge.

`goto` is the one place the graph stops rather than approximates. A `goto` — or
a `goto case` — is a node with no outgoing edge: the path ends there, because
following it is not modelled. Nothing after the jump is joined on, so the graph
never claims a path that does not exist. For the same reason a `goto` out of a
`try` is not shown running the enclosing `finally`, though at runtime it does:
the block would sit in front of a dead end and assert a path that goes nowhere.
The cost is at the other end — a labelled statement reached only by a `goto`
may not appear in the graph at all, because nothing the graph does follow leads
to it, and drawing it would mean a node with no way in.

A `switch` expression is drawn as a decision where it *is* the value: the
right-hand side of a `return`, an assignment or a declaration, an arrow body, or
another arm. Elsewhere it is one subexpression among several — an argument
(`Use(n switch {…})`), a `yield return`, an operand of a larger expression — and
its arms are flattened into the sequence of calls they make, like any other
branch nested inside an expression.

A `finally` runs on every way out of its `try`, including a `return`, `throw`,
`break` or `continue` that leaves early, so its block is drawn once per path
that runs it. Seeing the same `finally` body more than once in one graph is the
construct, not a bug.

### `helios summary [PATH]`

Generate a directory-level overview with symbol counts by language and kind.

```bash
helios summary
helios summary src/parsers/
```

Output is markdown by default, listing files and their exported symbols grouped by directory.

### `helios diff`

Show symbol changes (added, removed, modified) since the last indexed commit. Useful for surfacing what a working-tree change actually altered.

```bash
helios diff
```

### `helios status`

Report index health: last indexed commit, file/symbol counts, and staleness — the number of
indexable files whose contents on disk differ from what the index last parsed.

```bash
helios status
```

### `helios files [--language <LANG>]`

List indexed files with per-file symbol and import counts.

```bash
helios files
helios files --language rust
```

### `helios export [--limit <N>] [--offset <N>]`

Dump the entire index to markdown or JSON.

```bash
helios export > index.md
helios export --json > index.json
helios export --limit 500 --offset 1000
```

### Global Flags

- `--json` — Output results as JSON (available on all commands).
- `--compact` — Emit single-line JSON instead of pretty-printed (requires `--json`).
- `--quiet` — Suppress all stdout. Useful for scripting `init`/`update`.

### Exit Codes

- `0` — Success
- `1` — General error
- `2` — No index found (run `helios init` first)

## Architecture

```
main.rs              CLI entry point (clap)
commands/
  init.rs            Full indexing
  update.rs          Incremental indexing (git-aware)
  symbols.rs         Symbol search & filtering
  deps.rs            Dependency analysis (transitive via --depth)
  flow.rs            Control-flow graph of one function body
  summary.rs         Directory-level overview
  diff.rs            Symbol changes since last index
  status.rs          Index health & staleness
  files.rs           File-level metadata listing
  export.rs          Full index export
indexer.rs           Coordinates parsing and DB insertion
resolver.rs          Resolves import specifiers to indexed files
flow/
  mod.rs             Graph shape shared by every language, and the dispatch
  rust.rs            Rust statement mapping
  csharp.rs          C# statement mapping
parsers/
  mod.rs             Language detection & parser factory
  rust_parser.rs     Functions, structs, traits, enums, mods
  go.rs              Functions, structs, interfaces
  python.rs          Functions, classes, module constants
  typescript.rs      Functions, classes, types, interfaces, enums
  swift.rs           Classes, functions, structs
  csharp.rs          Classes, structs, records, interfaces, enums, methods, properties
db.rs                SQLite wrapper (files, symbols, imports, references)
git.rs               Git integration (HEAD, diff, repo detection)
sidecar.rs           Roslyn helper detection + invocation (semantic C#/XAML mode)
errors.rs            Typed errors (e.g. NoIndexError -> exit code 2)
helios-roslyn/       Optional .NET helper: Roslyn-based C# reference resolution
  Program.cs         Definitions and references from the C# compilations
  Xaml.cs            XAML `{Binding}` paths resolved against those compilations
```

### Database Schema

- **files** — Path, content hash (SHA256), language, last indexed timestamp
- **symbols** — Name, kind, visibility, scope, file reference, line/column
- **imports** — Source file, import path, alias, optional resolved file
- **import_names** — Local names each import binds (used to attribute usages)
- **references_** — Symbol reference locations across files, and whether the
  usage was `qualified` (reached through a receiver)
- **metadata** — Key-value store (e.g., `last_indexed_commit`)

Content hashing ensures unchanged files are skipped even without git.

## License

MIT
