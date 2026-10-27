use clap::{Parser, Subcommand, ValueEnum};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(name = "chunkify-analytics", version, about, propagate_version = true)]
pub struct Cli {
    /// Directory containing the database and credentials
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    /// Load environment values from this file instead of ./.env
    #[arg(long, global = true)]
    pub env_file: Option<PathBuf>,
    /// Load TOML configuration from this file instead of ./collector.toml
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize a new data directory and credentials
    Init,
    /// Run the analytics HTTP API and MCP endpoint
    Serve {
        /// Listening IP address and port
        #[arg(long)]
        bind: Option<SocketAddr>,
    },
    /// Run an optional stdio adapter for clients without HTTP MCP support
    Mcp,
    /// Manage projects
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage ingestion and read credentials
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// List projects, settings and key IDs
    List,
    /// Create a project and its initial credentials
    Create {
        id: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Change a project's display name
    Rename { id: String, name: String },
    /// Replace a project's overrides from a JSON file
    Configure { id: String, settings: String },
    /// Set retention in seconds, 0 for unlimited, or inherit
    Retention { id: String, value: String },
}

#[derive(Clone, ValueEnum)]
pub enum Permission {
    Public,
    Secret,
}

impl Permission {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Secret => "secret",
        }
    }
}

#[derive(Subcommand)]
pub enum KeyCommand {
    /// Create a named credential
    Create {
        project: String,
        permission: Permission,
        name: String,
    },
    /// Replace a credential's secret, retaining its key ID
    Rotate { project: String, key_id: String },
    /// Revoke a credential
    Revoke { project: String, key_id: String },
    /// Rotate the default project's primary credential
    RotatePrimary { permission: Permission },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Validate configuration without opening the database
    Check,
}

impl ProjectCommand {
    pub fn request(self) -> (&'static str, Vec<String>) {
        match self {
            Self::List => ("project-list", vec![]),
            Self::Create { id, name } => {
                let mut args = vec![id];
                args.extend(name);
                ("project-create", args)
            }
            Self::Rename { id, name } => ("project-rename", vec![id, name]),
            Self::Configure { id, settings } => ("project-configure", vec![id, settings]),
            Self::Retention { id, value } => ("project-retention", vec![id, value]),
        }
    }
}

impl KeyCommand {
    pub fn request(self) -> (&'static str, Vec<String>) {
        match self {
            Self::Create {
                project,
                permission,
                name,
            } => (
                "key-create",
                vec![project, permission.as_str().into(), name],
            ),
            Self::Rotate { project, key_id } => ("key-rotate", vec![project, key_id]),
            Self::Revoke { project, key_id } => ("key-revoke", vec![project, key_id]),
            Self::RotatePrimary { permission } => ("rotate-key", vec![permission.as_str().into()]),
        }
    }
}
