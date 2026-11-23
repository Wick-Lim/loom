use clap::Parser;
use loom_server::{ServerConfig, start_server};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "loom-server",
    version,
    about = "Project Loom native remote CAS and causal-ledger server"
)]
pub struct ServeArgs {
    /// Directory where remote Loom repositories (CAS + state.redb + refs) are stored
    #[arg(long, env = "LOOM_SERVER_DATA_DIR", default_value = "./loom-data")]
    pub data_dir: PathBuf,

    /// TCP address to listen on
    #[arg(long, env = "LOOM_SERVER_BIND", default_value = "127.0.0.1:9418")]
    pub bind: SocketAddr,

    /// Bearer token(s) accepted on /v1/* routes (repeatable or comma-separated via LOOM_SERVER_TOKEN)
    #[arg(long = "token", env = "LOOM_SERVER_TOKEN", value_delimiter = ',')]
    pub tokens: Vec<String>,

    /// Explicitly allow unauthenticated requests (development/local testing only)
    #[arg(long)]
    pub no_auth: bool,

    /// Disable automatic initialization of new repositories on first push
    #[arg(long)]
    pub no_auto_create: bool,
}

pub async fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = ServerConfig {
        bind: args.bind,
        data_dir: args.data_dir,
        tokens: args.tokens,
        allow_unauthenticated: args.no_auth,
        allow_auto_create: !args.no_auto_create,
        ..ServerConfig::default()
    };

    let handle = start_server(config).await?;
    eprintln!(
        "loom-server listening on http://{} (auth={})",
        handle.local_addr,
        if args.no_auth { "disabled" } else { "bearer" }
    );

    tokio::signal::ctrl_c().await?;
    eprintln!("shutting down loom-server...");
    handle.shutdown().await?;
    Ok(())
}

pub fn run_cli_from_args() -> anyhow::Result<()> {
    let args = ServeArgs::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_serve(args))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args = ServeArgs::parse();
    run_serve(args).await
}
