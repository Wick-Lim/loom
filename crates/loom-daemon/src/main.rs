use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "loomd", about = "Project Loom Single-Writer Daemon")]
struct DaemonArgs {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    idle_timeout_ms: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args = DaemonArgs::parse();
    let _ = loom_daemon::run_daemon(args.repo, args.idle_timeout_ms).await?;
    Ok(())
}
