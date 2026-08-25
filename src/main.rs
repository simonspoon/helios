mod commands;
mod db;
pub mod errors;
mod flow;
mod git;
mod indexer;
mod parsers;
mod resolver;
mod sidecar;

use clap::{Parser, Subcommand};

use errors::NoIndexError;

#[derive(Parser)]
#[command(
    name = "helios",
    version,
    about = "Code indexing tool for agent-driven codebase exploration",
    after_help = "EXIT CODES:\n  0  Success\n  1  General error\n  2  No index found (run `helios init` first)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Output in JSON format
    #[arg(long, global = true)]
    json: bool,

    /// Use compact single-line JSON (requires --json)
    #[arg(long, global = true)]
    compact: bool,

    /// Suppress all output (overrides --json)
    #[arg(long, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Full index of project -> .helios/index.db
    Init {
        /// Roslyn analyze timeout in seconds (for large C# repos)
        #[arg(
            long,
            env = "HELIOS_ANALYZE_TIMEOUT",
            value_parser = clap::value_parser!(u64).range(1..),
            default_value_t = sidecar::ANALYZE_TIMEOUT.as_secs()
        )]
        timeout: u64,
    },
    /// Incremental update (git diff-based)
    Update,
    /// List symbols in the index
    Symbols {
        /// Filter by file path
        #[arg(long)]
        file: Option<String>,
        /// Filter by symbol kind (fn, struct, trait, enum, class, interface, type, const, mod)
        #[arg(long)]
        kind: Option<String>,
        /// Filter by name pattern (regex)
        #[arg(long)]
        grep: Option<String>,
        /// Filter by scope (e.g. impl block or class name)
        #[arg(long)]
        scope: Option<String>,
        /// Filter by visibility (pub or private)
        #[arg(long)]
        visibility: Option<String>,
        /// Only symbols with a parameter whose source spelling contains this
        #[arg(long)]
        param: Option<String>,
        /// Only symbols whose return or declared type contains this
        #[arg(long)]
        returns: Option<String>,
        /// Show symbol body/source code
        #[arg(long)]
        body: bool,
        /// Maximum number of symbols to return
        #[arg(long)]
        limit: Option<i64>,
        /// Number of symbols to skip
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Show dependencies for a symbol or file
    Deps {
        /// File path, symbol name, or one definition of it (Class.Method, path/to/file.ts:name)
        target: String,
        /// Restrict a symbol target to definitions in this scope (class or impl block)
        #[arg(long)]
        scope: Option<String>,
        /// Restrict a symbol target to definitions in files matching this path
        #[arg(long)]
        file: Option<String>,
        /// Transitive traversal depth (default: 1, file targets only)
        #[arg(long, default_value = "1")]
        depth: u32,
    },
    /// Control-flow graph of one function body (Rust and C#)
    Flow {
        /// Function or method name (Class.Method, path/to/file.rs:name)
        target: String,
        /// Restrict the target to definitions in this scope (class or impl block)
        #[arg(long)]
        scope: Option<String>,
        /// Restrict the target to definitions in files matching this path
        #[arg(long)]
        file: Option<String>,
        /// Select the definition declared on this line (tells overloads apart)
        #[arg(long)]
        line: Option<i64>,
        /// Emit a mermaid flowchart instead of an indented tree
        #[arg(long)]
        mermaid: bool,
    },
    /// Directory-level overview
    Summary {
        /// Path to summarize (defaults to project root)
        path: Option<String>,
    },
    /// Show symbol changes since last index
    Diff,
    /// Show index status and staleness info
    Status,
    /// List indexed files with symbol/import counts
    Files {
        /// Filter by language (e.g. rust, python, go)
        #[arg(long)]
        language: Option<String>,
    },
    /// Dump full index to markdown
    Export {
        /// Maximum number of symbols to return
        #[arg(long)]
        limit: Option<i64>,
        /// Number of symbols to skip
        #[arg(long)]
        offset: Option<i64>,
    },
}

fn main() {
    let cli = Cli::parse();

    let compact = cli.compact;

    let result = match &cli.command {
        Command::Init { timeout } => commands::init::run(cli.json, compact, cli.quiet, *timeout),
        Command::Update => commands::update::run(cli.json, compact, cli.quiet),
        Command::Symbols {
            file,
            kind,
            grep,
            scope,
            visibility,
            param,
            returns,
            body,
            limit,
            offset,
        } => commands::symbols::run(
            file.as_deref(),
            kind.as_deref(),
            grep.as_deref(),
            scope.as_deref(),
            visibility.as_deref(),
            param.as_deref(),
            returns.as_deref(),
            cli.json,
            compact,
            *body,
            *limit,
            *offset,
        ),
        Command::Files { language } => commands::files::run(language.as_deref(), cli.json, compact),
        Command::Diff => commands::diff::run(cli.json, compact),
        Command::Deps {
            target,
            scope,
            file,
            depth,
        } => commands::deps::run(
            target,
            cli.json,
            compact,
            *depth,
            scope.as_deref(),
            file.as_deref(),
        ),
        Command::Flow {
            target,
            scope,
            file,
            line,
            mermaid,
        } => commands::flow::run(
            target,
            cli.json,
            compact,
            *mermaid,
            scope.as_deref(),
            file.as_deref(),
            *line,
        ),
        Command::Summary { path } => commands::summary::run(path.as_deref(), cli.json, compact),
        Command::Status => commands::status::run(cli.json, compact),
        Command::Export { limit, offset } => {
            commands::export::run(cli.json, compact, *limit, *offset)
        }
    };

    if let Err(e) = result {
        let exit_code = if e.downcast_ref::<NoIndexError>().is_some() {
            2
        } else {
            1
        };
        if cli.json {
            let err = serde_json::json!({"error": e.to_string()});
            let formatted = if compact {
                serde_json::to_string(&err).unwrap_or_default()
            } else {
                serde_json::to_string_pretty(&err).unwrap_or_default()
            };
            eprintln!("{}", formatted);
        } else {
            eprintln!("error: {e:#}");
        }
        std::process::exit(exit_code);
    }
}
