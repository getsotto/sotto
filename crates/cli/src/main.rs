//! The `sotto` CLI. See `docs/CLI.md`. M0 wires up the command surface; behaviour lands in M2.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "sotto", version, about = "End-to-end-encrypted secret sync for teams")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Wire up the current repo (writes .secrets.yaml).
    Init,
    /// Run a command with secrets injected as env vars — the hot path.
    Run {
        /// The command and its arguments, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => eprintln!("sotto init: not yet implemented (M2)"),
        Command::Run { args } => {
            eprintln!("sotto run {args:?}: not yet implemented (M2)")
        }
    }
}
