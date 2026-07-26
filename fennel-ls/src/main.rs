mod analyzer;
mod config;
mod lua_to_fennel;
mod docs;
mod expander;
mod fmt;
mod lexer;
mod parser;
mod server;
mod text;
mod workspace;

use clap::{Parser, Subcommand};
use tower_lsp::{LspService, Server};

#[derive(Parser)]
#[command(name = "fennel-ls", version, about = "Language server for Fennel")]
struct Cli {
    /// Disable textDocument/formatting support.
    #[arg(long)]
    no_formatting: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the LSP server (default when no subcommand given)
    Server,
    /// Check files for errors and print diagnostics
    Check { files: Vec<std::path::PathBuf> },
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let cli = Cli::parse();

    let formatting_enabled = !cli.no_formatting;

    match cli.command.unwrap_or(Command::Server) {
        Command::Server => run_server(formatting_enabled).await,
        Command::Check { files } => run_check(files),
    }
}

async fn run_server(formatting_enabled: bool) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(move |client| server::Backend::new(client, formatting_enabled));
    Server::new(stdin, stdout, socket).serve(service).await;
}

fn run_check(files: Vec<std::path::PathBuf>) {
    let mut had_errors = false;

    for path in &files {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{}: {}", path.display(), e);
                had_errors = true;
                continue;
            }
        };

        let (ast, parse_errors) = parser::Parser::parse(&text);
        let analysis = analyzer::analyze(&ast);

        for err in &parse_errors {
            println!(
                "{}:{}:{}: error: {}",
                path.display(),
                err.span.line + 1,
                err.span.col + 1,
                err.message
            );
            had_errors = true;
        }

        let builtins = docs::default_set();
        for sym in &analysis.syms {
            if sym.is_def {
                continue;
            }
            if sym.def_byte.is_none() && !builtins.is_known(&sym.name) {
                let root = sym.name.split(['.', ':']).find(|s| !s.is_empty()).unwrap_or(&sym.name);
                if !server::known_global(root) {
                    println!(
                        "{}:{}:{}: warning: unknown identifier `{}`",
                        path.display(),
                        sym.span.line + 1,
                        sym.span.col + 1,
                        sym.name
                    );
                }
            }
        }
    }

    if had_errors {
        std::process::exit(1);
    }
}
