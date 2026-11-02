use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Kanso: a single-user kanban board.
#[derive(Debug, Parser)]
#[command(name = "kanso", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the Kanso HTTP server.
    Serve {
        /// TCP port to listen on (overrides the port in KANSO_BIND).
        #[arg(long)]
        port: Option<u16>,
        /// Directory holding kanso.db (overrides KANSO_DATA_DIR).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}
