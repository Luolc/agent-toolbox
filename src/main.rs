//! `atb`: command-line tools for running and operating AI coding agents
//! (Claude Code, Codex, Grok Build). The command tree is tool first, target
//! harness second: `atb quota <harness>` reads the subscription quota. Tools
//! that target no harness take their own subcommands, as in `atb linear claim`.

mod common;
mod linear;
mod quota;
mod timefmt;

use std::process::ExitCode;

use clap::Parser;

use common::Error;

const EXIT_ERROR: u8 = 1;
const EXIT_RATE_LIMITED: u8 = 2;

#[derive(Parser)]
#[command(name = "atb", version, about)]
struct Cli {
    #[command(subcommand)]
    tool: Tool,
}

#[derive(clap::Subcommand)]
enum Tool {
    /// Subscription quota as the vendor reports it: window percentages and
    /// reset times. These are not token ledgers and not bills.
    Quota(quota::Args),
    /// Claim and release Linear issues by comment, create issues for agents,
    /// and run read-only GraphQL queries.
    Linear(linear::Args),
}

fn main() -> ExitCode {
    let result = match Cli::parse().tool {
        Tool::Quota(args) => quota::run(args).map(|()| 0),
        Tool::Linear(args) => linear::run(args),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(match err {
                Error::RateLimited { .. } => EXIT_RATE_LIMITED,
                Error::Usage(_) => EXIT_ERROR,
            })
        }
    }
}
