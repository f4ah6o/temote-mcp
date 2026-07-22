mod approvals;
mod config;
mod mcp;
mod sandbox;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdin/stdout (the default command).
    Mcp,
    /// Interactively approve or deny privileged tool calls.
    Approvals,
    /// Permanently allow sandboxed writes and commands rooted in a directory.
    Permit { directory: PathBuf },
    /// Remove a directory from the permanent allow-list.
    Revoke { directory: PathBuf },
    /// Show permanently permitted directories.
    Permits,
    /// Set the default working directory used by the MCP server.
    SetCwd { directory: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Mcp) {
        Command::Mcp => mcp::serve().await,
        Command::Approvals => approvals::run_ui().await,
        Command::Permit { directory } => {
            let path = config::permit(&directory).await?;
            println!("permitted {}", path.display());
            Ok(())
        }
        Command::Revoke { directory } => {
            let path = config::revoke(&directory).await?;
            println!("revoked {}", path.display());
            Ok(())
        }
        Command::Permits => {
            for path in config::load().await?.permitted_directories {
                println!("{}", path.display());
            }
            Ok(())
        }
        Command::SetCwd { directory } => {
            let path = config::set_default_cwd(&directory).await?;
            println!("default cwd {}", path.display());
            Ok(())
        }
    }
}
