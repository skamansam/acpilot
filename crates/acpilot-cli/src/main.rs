//! `acpilot` — the command line.
//!
//! P1 ships only what is honest to ship: `--version` and `version`. Every other
//! command arrives in P5, once the daemon has an API to talk to. Commands are
//! added here as typed calls into `acpilot-proto`, never by reimplementing
//! daemon behaviour locally.

use acpilot_proto::API_PREFIX;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "acpilot",
    version,
    about = "Edit and run ACP agents over one daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print version and protocol information.
    Version,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Version) => {
            println!("acpilot {}", env!("CARGO_PKG_VERSION"));
            println!("api      {}", API_PREFIX);
        }
        None => {
            // `clap` prints usage on --help; with no subcommand, do the same rather
            // than pretending to do work.
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let _ = cmd.print_help();
            println!();
            std::process::exit(i32::from(acpilot_core::error::exit::USAGE));
        }
    }
}
