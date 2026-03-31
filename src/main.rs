mod commands;
mod db;
mod git;
mod indexer;
mod parsers;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "helios",
    version,
    about = "Code indexing tool for agent-driven codebase exploration"
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
}

#[derive(Subcommand)]
enum Command {
    /// Full index of project -> .helios/index.db
    Init,
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
    },
    /// Show dependencies for a symbol or file
    Deps {
        /// Symbol name or file path to query
        target: String,
    },
    /// Directory-level overview
    Summary {
        /// Path to summarize (defaults to project root)
        path: Option<String>,
    },
    /// Dump full index to markdown
    Export,
}

fn main() {
    let cli = Cli::parse();

    let compact = cli.compact;

    let result = match &cli.command {
        Command::Init => commands::init::run(cli.json, compact),
        Command::Update => commands::update::run(cli.json, compact),
        Command::Symbols { file, kind, grep } => commands::symbols::run(
            file.as_deref(),
            kind.as_deref(),
            grep.as_deref(),
            cli.json,
            compact,
        ),
        Command::Deps { target } => commands::deps::run(target, cli.json, compact),
        Command::Summary { path } => commands::summary::run(path.as_deref(), cli.json, compact),
        Command::Export => commands::export::run(cli.json, compact),
    };

    if let Err(e) = result {
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
        std::process::exit(1);
    }
}
