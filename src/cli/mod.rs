use std::fmt;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

mod client;
mod commands;
mod config;
mod output;
mod secret;
mod serve;

/// Kanso: a single-user kanban board.
#[derive(Debug, Parser)]
#[command(name = "kanso", version, about)]
pub struct Cli {
    /// Print the API's JSON response unchanged, for machines.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Request and use a one-time secret handoff.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Run the Kanso HTTP server.
    Serve {
        /// TCP port to listen on (overrides the port in KANSO_BIND).
        #[arg(long)]
        port: Option<u16>,
        /// Directory holding kanso.db (overrides KANSO_DATA_DIR).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Read, validate and write the CLI configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// List, create and edit projects.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// List, show, create and edit cards.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Show the next claimable card.
    Next {
        /// Restrict the search to one project key.
        #[arg(long)]
        project: Option<String>,
        /// Claim the card atomically before printing it.
        #[arg(long)]
        claim: bool,
        /// Attribution name sent as X-Kanso-Agent.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Claim a card.
    Claim {
        reference: String,
        /// Attribution name sent as X-Kanso-Agent.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Release a claim without changing the column.
    Release { reference: String },
    /// Move a card, enter the blocker column with a reason, or restore it.
    Move {
        reference: String,
        target: MoveTarget,
        /// Stored in blocked_reason; only valid when moving to blocker.
        #[arg(long)]
        reason: Option<String>,
        /// Attribution name sent as X-Kanso-Agent.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Add a comment. A text of "-" reads standard input.
    Comment {
        reference: String,
        text: String,
        /// Attribution name sent as X-Kanso-Agent.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Add a checklist item, tick one by number, or list the checklist.
    Check {
        reference: String,
        /// Append a checklist item.
        #[arg(long)]
        add: Option<String>,
        /// Mark the 1-based item number done.
        #[arg(long)]
        done: Option<usize>,
        /// List the checklist.
        #[arg(long)]
        list: bool,
        /// Attribution name sent as X-Kanso-Agent.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Record that a card depends on another card (human token).
    Depend {
        reference: String,
        on: String,
        dependency: String,
    },
    /// Propose a card through the inbox (an agent token always proposes).
    Propose {
        title: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        description: Option<String>,
        /// Attribution name sent as X-Kanso-Agent.
        #[arg(long)]
        agent: Option<String>,
    },
    /// List proposed cards.
    Inbox,
    /// Link an existing agent session to a card.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Create, list and revoke API tokens.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Print or install the agent skill.
    Skill {
        /// Print the embedded skill to stdout (the default).
        #[arg(long, conflicts_with = "install")]
        print: bool,
        /// Install it under <dir>/kanso/SKILL.md.
        #[arg(long, conflicts_with = "print")]
        install: bool,
        /// Install directory (default ~/.config/opencode/skills).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Write a server-side SQLite backup.
    Backup {
        /// Server-side directory under the data directory.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Request a secret; label and purpose are public metadata.
    Request {
        reference: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        purpose: String,
    },
    /// Inspect public handoff metadata.
    Status { request: String },
    /// Cancel a handoff.
    Cancel { request: String },
    /// Consume once and directly run a child with an environment variable.
    Exec {
        request: String,
        #[arg(long)]
        env: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Validate a URL and token with GET /api/me, then write the config file.
    Init {
        #[arg(long)]
        url: String,
        #[arg(long)]
        token: String,
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// List active projects.
    List,
    /// Create a project.
    Add {
        #[arg(long)]
        key: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        repo_path: Option<String>,
        #[arg(long)]
        color: Option<String>,
    },
    /// Show one project by key or id.
    Show { reference: String },
    /// Edit a project.
    Edit {
        reference: String,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        repo_path: Option<String>,
        #[arg(long)]
        color: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// List cards.
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        label: Option<String>,
        /// Only cards claimed by the authenticated token.
        #[arg(long)]
        mine: bool,
    },
    /// Show a card with its checklist, comments, activity and dependencies.
    Show { reference: String },
    /// Create a card.
    Add {
        title: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        priority: Option<String>,
    },
    /// Edit a card's title, description or due date.
    Edit {
        reference: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        due: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// Link an existing session id and directory to a card.
    Link {
        reference: String,
        session_id: String,
        #[arg(long)]
        directory: String,
        #[arg(long)]
        status: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Create a token and print it once.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, value_parser = ["human", "agent"])]
        scope: String,
    },
    /// List tokens without their secrets.
    List,
    /// Revoke a token by name.
    Revoke { name: String },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum MoveTarget {
    #[value(name = "backlog")]
    Backlog,
    #[value(name = "todo")]
    Todo,
    #[value(name = "in_progress")]
    InProgress,
    #[value(name = "in_review")]
    InReview,
    #[value(name = "blocker")]
    Blocker,
    #[value(name = "done")]
    Done,
    #[value(name = "restore")]
    Restore,
}

impl MoveTarget {
    fn status(self) -> Option<&'static str> {
        match self {
            Self::Backlog => Some("backlog"),
            Self::Todo => Some("todo"),
            Self::InProgress => Some("in_progress"),
            Self::InReview => Some("in_review"),
            Self::Blocker => Some("blocker"),
            Self::Done => Some("done"),
            Self::Restore => None,
        }
    }
}

/// A CLI failure, with the exit code PLAN §6 assigns to its class.
#[derive(Debug)]
pub enum CliError {
    Child(u8),
    /// Bad arguments, missing configuration or a start-up failure: 1.
    Usage(String),
    /// The server answered with an error status.
    Api {
        status: u16,
        code: String,
        message: String,
    },
    /// The server could not be reached: 2.
    Network(String),
    /// A local file or stream failed: 2.
    Io(String),
    /// The server failed to start: 1.
    Startup(String),
}

impl CliError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Child(code) => *code,
            Self::Usage(_) | Self::Startup(_) => 1,
            // PLAN §6 groups every conflict, including a rate-limited request,
            // as exit 3; all other API failures are 2.
            Self::Api { status, .. } if *status == 409 || *status == 429 => 3,
            Self::Api { .. } | Self::Network(_) | Self::Io(_) => 2,
        }
    }

    pub fn print(&self) {
        if matches!(self, Self::Child(_)) {
            return;
        }
        output::error_line(format_args!("kanso: {self}"));
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Child(code) => write!(f, "child exited with status {code}"),
            Self::Usage(message) => write!(f, "usage: {message}"),
            Self::Api { code, message, .. } => write!(f, "{code}: {message}"),
            Self::Network(message) => write!(f, "network: {message}"),
            Self::Io(message) => write!(f, "io: {message}"),
            Self::Startup(message) => write!(f, "error: {message}"),
        }
    }
}

impl From<CliError> for std::process::ExitCode {
    fn from(error: CliError) -> Self {
        error.print();
        std::process::ExitCode::from(error.exit_code())
    }
}

pub fn run(cli: Cli) -> Result<(), CliError> {
    let Cli { json, command } = cli;
    let result = match command {
        Command::Secret { command } => secret::run(command, json),
        Command::Serve { port, data_dir } => serve::run(port, data_dir),
        Command::Config { command } => commands::config(command, json),
        Command::Project { command } => commands::project(command, json),
        Command::Task { command } => commands::task(command, json),
        Command::Next {
            project,
            claim,
            agent,
        } => commands::next(project, claim, agent, json),
        Command::Claim { reference, agent } => commands::claim(&reference, agent, json),
        Command::Release { reference } => commands::release(&reference, json),
        Command::Move {
            reference,
            target,
            reason,
            agent,
        } => commands::move_(reference, target, reason, agent, json),
        Command::Comment {
            reference,
            text,
            agent,
        } => commands::comment(reference, text, agent, json),
        Command::Check {
            reference,
            add,
            done,
            list,
            agent,
        } => commands::check(reference, add, done, list, agent, json),
        Command::Depend {
            reference,
            on,
            dependency,
        } => commands::depend(reference, on, dependency, json),
        Command::Propose {
            title,
            project,
            description,
            agent,
        } => commands::propose(title, project, description, agent, json),
        Command::Inbox => commands::inbox(json),
        Command::Session { command } => commands::session(command, json),
        Command::Token { command } => commands::token(command, json),
        Command::Skill {
            print,
            install,
            dir,
        } => commands::skill(print, install, dir, json),
        Command::Backup { out } => commands::backup(out, json),
    };
    result?;
    output::finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16) -> CliError {
        CliError::Api {
            status,
            code: "code".to_owned(),
            message: "message".to_owned(),
        }
    }

    #[test]
    fn exit_codes_follow_the_plan() {
        assert_eq!(CliError::usage("bad").exit_code(), 1);
        assert_eq!(CliError::Startup("bad".to_owned()).exit_code(), 1);
        for status in [400, 401, 403, 404, 500, 503] {
            assert_eq!(api(status).exit_code(), 2, "{status}");
        }
        assert_eq!(api(409).exit_code(), 3);
        assert_eq!(api(429).exit_code(), 3);
        assert_eq!(CliError::Network("down".to_owned()).exit_code(), 2);
        assert_eq!(CliError::Io("disk".to_owned()).exit_code(), 2);
    }

    #[test]
    fn errors_render_the_code_and_message() {
        assert_eq!(api(409).to_string(), "code: message");
        assert_eq!(
            CliError::Network("down".to_owned()).to_string(),
            "network: down"
        );
    }
}
