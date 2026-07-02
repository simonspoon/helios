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

Helios works fully without the helper — when it is absent, `helios init` silently
uses the tree-sitter path. Nothing else changes.

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

Run `helios init` in a C# project, then:

```bash
helios status
# ...
# C# resolver: roslyn
```

`C# resolver: treesitter` means the helper wasn't used — a `warning:` line during
`init` says why (dotnet missing, runtime below .NET 8, helper failed). A missing
helper with no `HELIOS_ROSLYN` set falls back silently by design.

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
- `--body` — Include each symbol's source body in the output
- `--limit <N>` — Maximum number of symbols to return
- `--offset <N>` — Number of symbols to skip

Output format:

```
src/main.rs:42:0 fn pub main
src/lib.rs:10:4 struct pub Parser
```

### `helios deps <TARGET> [--depth <N>]`

Show dependencies and dependents for a symbol or file. Auto-detects the target type: paths containing `/` or `.` are treated as files, otherwise as symbols.

```bash
# File dependencies
helios deps "src/parser.rs"
# Dependencies (what src/parser.rs imports):
#   src/parser.rs -> std::collections (import)
# Dependents (what imports src/parser.rs):
#   src/main.rs -> src/parser.rs (import)

# Transitive file dependencies (depth 3)
helios deps "src/parser.rs" --depth 3

# Symbol references
helios deps "parse_token"
```

- `--depth <N>` — Transitive traversal depth (default: 1, file targets only)

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

Report index health: last indexed commit, file/symbol counts, and staleness vs. the working tree.

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
  summary.rs         Directory-level overview
  diff.rs            Symbol changes since last index
  status.rs          Index health & staleness
  files.rs           File-level metadata listing
  export.rs          Full index export
indexer.rs           Coordinates parsing and DB insertion
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
sidecar.rs           Roslyn helper detection + invocation (semantic C# mode)
errors.rs            Typed errors (e.g. NoIndexError -> exit code 2)
helios-roslyn/       Optional .NET helper: Roslyn-based C# reference resolution
```

### Database Schema

- **files** — Path, content hash (SHA256), language, last indexed timestamp
- **symbols** — Name, kind, visibility, scope, file reference, line/column
- **imports** — Source file, import path, alias, optional resolved file
- **references_** — Symbol reference locations across files
- **metadata** — Key-value store (e.g., `last_indexed_commit`)

Content hashing ensures unchanged files are skipped even without git.

## License

MIT
