//! `teams` CLI parser and command dispatcher.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::AppConfig;

#[derive(Parser, Debug)]
#[command(
    name = "teams",
    version,
    about = "Microsoft Teams 터미널 클라이언트 (CLI · TUI · HTTP API · MCP)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Doctor,
}

pub async fn run_from_env() -> Result<()> {
    let _cfg = AppConfig::load();
    let _cli = Cli::parse();
    Ok(())
}
