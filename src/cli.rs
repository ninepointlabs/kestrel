use clap::{Parser, Subcommand};

/// Post to X (Twitter) from the command line or from AI agents over MCP.
///
/// Credentials live in ~/.config/kestrel/config.toml (run `kestrel configure`).
/// Every post counts against a hard daily limit tracked in
/// ~/.config/kestrel/state.json, shared by the CLI and the MCP server.
#[derive(Debug, Parser)]
#[command(name = "kestrel", version, about, long_about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Post a tweet
    #[command(after_help = "Example:\n  kestrel post \"Hello world\"")]
    Post {
        /// Text of the tweet
        text: String,
    },

    /// Show today's post count against the daily limit
    Status,

    /// Interactively set API credentials and the daily post limit
    Configure,

    /// Run as an MCP server over stdio (for Hermes and other AI agents)
    Serve,
}
