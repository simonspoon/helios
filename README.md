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

## Usage

### `helios init`

Create a full index of the project. Walks all files (respecting `.gitignore`), parses symbols with tree-sitter, and stores results in `.helios/index.db`.

```bash
helios init
# Indexed 42 files (156 symbols) in 2.34s
# Database: /path/to/.helios/index.db
```

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

# Search by name
helios symbols --grep "parse"

# Combine filters
helios symbols --kind struct --file src/parsers/
```

Options:
- `--file <PATH>` — Filter by file path (substring match)
- `--kind <KIND>` — Filter by symbol kind: `fn`, `struct`, `trait`, `enum`, `class`, `interface`, `type`, `const`, `mod`
- `--grep <PATTERN>` — Filter by symbol name (substring match)
- `--json` — JSON output

Output format:

```
src/main.rs:42:0 fn pub main
src/lib.rs:10:4 struct pub Parser
```

### `helios deps <TARGET>`

Show dependencies and dependents for a symbol or file. Auto-detects the target type: paths containing `/` or `.` are treated as files, otherwise as symbols.

```bash
# File dependencies
helios deps "src/parser.rs"
# Dependencies (what src/parser.rs imports):
#   src/parser.rs -> std::collections (import)
# Dependents (what imports src/parser.rs):
#   src/main.rs -> src/parser.rs (import)

# Symbol references
helios deps "parse_token"
```

### `helios summary [PATH]`

Generate a directory-level overview with symbol counts by language and kind.

```bash
helios summary
helios summary src/parsers/
```

Output is markdown by default, listing files and their exported symbols grouped by directory.

### `helios export`

Dump the entire index to markdown or JSON.

```bash
helios export > index.md
helios export --json > index.json
```

### Global Flag

- `--json` — Available on all commands. Output results as JSON.

## Architecture

```
main.rs              CLI entry point (clap)
commands/
  init.rs            Full indexing
  update.rs          Incremental indexing (git-aware)
  symbols.rs         Symbol search & filtering
  deps.rs            Dependency analysis
  summary.rs         Directory-level overview
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
