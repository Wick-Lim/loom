mod client;
mod commands;
mod tui;

use clap::{Parser, Subcommand};
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "loom", version, about = "Project Loom Local-First Multi-Agent VCS")]
struct Cli {
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Internal hidden daemon entrypoint
    #[command(name = "__daemon", hide = true)]
    Daemon {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        idle_timeout_ms: Option<u64>,
    },

    /// Initialize a Loom repository in .loom/
    Init {
        #[arg(long)]
        colocate_git: bool,
    },

    /// List viewports
    List {
        #[arg(long)]
        include_gc: bool,
    },

    /// Show daemon & repository status
    Status,

    /// Show operation & capsule log
    Log {
        #[arg(long)]
        capsules: bool,
        #[arg(long)]
        symbol: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Undo an operation
    Undo {
        #[arg(long)]
        op: Option<u64>,
    },

    /// Garbage collect obsolete viewports, unreferenced CAS blobs, and compacted WAL segments
    Gc {
        #[arg(long)]
        prune_viewports: bool,
    },

    /// Spawn an isolated CoW viewport (and optionally execute a command inside it)
    Spawn {
        #[arg(short = 'n', long)]
        name: String,
        #[arg(short = 'm', long = "intent", alias = "message")]
        intent: String,
        #[arg(last = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },

    /// Inspect or watch real-time Speculative Merge Radar status
    Radar {
        /// Continuously watch live radar updates (interactive TUI unless --json is set)
        #[arg(long)]
        watch: bool,
        /// Filter to a specific viewport name or ID
        #[arg(long)]
        viewport: Option<String>,
    },

    /// Model Context Protocol (MCP) server commands
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },

    /// Verify and seal a viewport's changes into a CausalCapsule and merge to trunk
    Seal {
        #[arg(long)]
        viewport: Option<String>,
        #[arg(short = 'v', long = "verify")]
        verify: Option<String>,
        #[arg(short = 'm', long = "summary", alias = "message")]
        summary: Option<String>,
    },

    /// Record a NegativeEdge failure lesson, clean-restore a viewport, and mark it dropped
    Drop {
        #[arg(long)]
        viewport: Option<String>,
        #[arg(short = 'r', long = "reason")]
        reason: String,
        /// Failure category (CompileError, TestFailure, SemanticConflict, StructuralConflict, PerformanceRegression, DesignDeadEnd, Deadlock, Other)
        #[arg(short = 'k', long = "kind")]
        kind: Option<String>,
    },

    /// Bidirectional Git projection commands (gix)
    Git {
        #[command(subcommand)]
        command: GitCommands,
    },
}

#[derive(Debug, Subcommand)]
enum GitCommands {
    /// Push sealed CausalCapsules and NegativeEdge notes to a local/file:// bare repository
    Push {
        remote: Option<String>,
        branch: Option<String>,
    },
    /// Pull commits and refs/notes/loom from a local/file:// bare repository and rebase active viewports
    Pull {
        remote: Option<String>,
        branch: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommands {
    /// Start the embedded MCP server (stdio by default, or Streamable HTTP with --http)
    Serve {
        /// Default viewport name or ID for MCP tool calls
        #[arg(long)]
        viewport: Option<String>,
        /// Listen address for Streamable HTTP transport (e.g. 127.0.0.1:8787)
        #[arg(long)]
        http: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    if let Some(arg0) = std::env::args_os().next()
        && Path::new(&arg0).file_stem() == Some(OsStr::new("loomd"))
    {
        return loom_daemon::run_cli_from_args();
    }

    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let exit_code = rt.block_on(async {
        match cli.command {
            Commands::Daemon {
                repo,
                idle_timeout_ms,
            } => {
                tracing_subscriber::fmt()
                    .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                    .with_writer(std::io::stderr)
                    .init();
                let _ = loom_daemon::run_daemon(repo, idle_timeout_ms).await?;
                Ok(0)
            }
            Commands::Init { colocate_git } => {
                commands::cmd_init(cli.repo.as_deref(), colocate_git, cli.json).await?;
                Ok(0)
            }
            Commands::List { include_gc } => {
                commands::cmd_list(cli.repo.as_deref(), include_gc, cli.json).await?;
                Ok(0)
            }
            Commands::Status => {
                commands::cmd_status(cli.repo.as_deref(), cli.json).await?;
                Ok(0)
            }
            Commands::Log {
                capsules,
                symbol,
                limit,
            } => {
                commands::cmd_log(cli.repo.as_deref(), capsules, symbol, limit, cli.json).await?;
                Ok(0)
            }
            Commands::Undo { op } => {
                commands::cmd_undo(cli.repo.as_deref(), op, cli.json).await?;
                Ok(0)
            }
            Commands::Gc { prune_viewports } => {
                commands::cmd_gc(cli.repo.as_deref(), prune_viewports, cli.json).await?;
                Ok(0)
            }
            Commands::Spawn { name, intent, cmd } => {
                commands::cmd_spawn(cli.repo.as_deref(), name, intent, cmd, cli.json).await
            }
            Commands::Radar { watch, viewport } => {
                commands::cmd_radar(cli.repo.as_deref(), watch, viewport, cli.json).await?;
                Ok(0)
            }
            Commands::Mcp { command } => match command {
                McpCommands::Serve { viewport, http } => {
                    tracing_subscriber::fmt()
                        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                        .with_writer(std::io::stderr)
                        .init();
                    commands::cmd_mcp_serve(cli.repo.as_deref(), viewport, http).await?;
                    Ok(0)
                }
            },
            Commands::Seal {
                viewport,
                verify,
                summary,
            } => {
                commands::cmd_seal(cli.repo.as_deref(), viewport, verify, summary, cli.json)
                    .await?;
                Ok(0)
            }
            Commands::Drop {
                viewport,
                reason,
                kind,
            } => {
                commands::cmd_drop(cli.repo.as_deref(), viewport, reason, kind, cli.json).await?;
                Ok(0)
            }
            Commands::Git { command } => match command {
                GitCommands::Push { remote, branch } => {
                    commands::cmd_git_push(cli.repo.as_deref(), remote, branch, cli.json).await?;
                    Ok(0)
                }
                GitCommands::Pull { remote, branch } => {
                    commands::cmd_git_pull(cli.repo.as_deref(), remote, branch, cli.json).await?;
                    Ok(0)
                }
            },
        }
    })?;

    drop(rt);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
