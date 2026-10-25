//! `acpilot` — the command line.
//!
//! Everything testable lives in `acpilot-cli`'s library; this file parses and
//! turns the returned code into the process exit status.

use acpilot_cli::cli::{Cli, run};
use clap::{CommandFactory, FromArgMatches};

fn main() {
    // `Usage:` is rendered from the name the process was invoked as, so on
    // Windows it read `acpilot.exe` — in help text, in the snapshots and in
    // anything that scrapes the first line. The command is `acpilot` everywhere
    // it is documented, so it is named once here instead of per platform.
    let matches = Cli::command().bin_name("acpilot").get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    std::process::exit(i32::from(run(cli)));
}
