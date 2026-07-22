mod approvals;
mod config;
mod mcp;
mod sandbox;

use anyhow::Result;
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a session in the current directory and show its permission UI.
    Start,
    /// Run the MCP server for a session over stdin/stdout.
    Mcp { session_id: Uuid },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start) {
        Command::Start => approvals::start().await,
        Command::Mcp { session_id } => mcp::serve(session_id).await,
    }
}
