//! `acpilot` — the command line.
//!
//! Everything testable lives in `acpilot-cli`'s library; this file parses and
//! turns the returned code into the process exit status.

use acpilot_cli::cli::{Cli, run};
use clap::Parser;

fn main() {
    std::process::exit(i32::from(run(Cli::parse())));
}
