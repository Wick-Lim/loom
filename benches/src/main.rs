mod fixture;
mod guard;
mod ipc;
mod loom_cli;
mod metrics;
mod report;
mod stats;
mod win32;

use anyhow::Context;
use guard::BenchWorkspaceGuard;
use loom_cli::LoomCli;
use metrics::BenchmarkReport;
use std::path::PathBuf;
use std::process::Command;

struct CliArgs {
    loom_bin: PathBuf,
    report_path: Option<PathBuf>,
    json_out: Option<PathBuf>,
    quick: bool,
}

fn parse_args() -> anyhow::Result<CliArgs> {
    let mut loom_bin = PathBuf::from("target/release/loom.exe");
    let mut report_path = None;
    let mut json_out = None;
    let mut quick = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--loom-bin" => {
                let val = args.next().context("--loom-bin requires a path")?;
                loom_bin = PathBuf::from(val);
            }
            "--report" => {
                let val = args.next().context("--report requires a path")?;
                report_path = Some(PathBuf::from(val));
            }
            "--json-out" => {
                let val = args.next().context("--json-out requires a path")?;
                json_out = Some(PathBuf::from(val));
            }
            "--quick" => {
                quick = true;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: loom-bench [--loom-bin <PATH>] [--report <PATH>] [--json-out <PATH>] [--quick]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    let abs_bin = if loom_bin.is_absolute() {
        loom_bin
    } else {
        std::env::current_dir()?.join(loom_bin)
    };
    anyhow::ensure!(
        abs_bin.is_file(),
        "loom binary not found at {} (build with `cargo build --release --package loom-cli --bin loom`)",
        abs_bin.display()
    );

    Ok(CliArgs {
        loom_bin: abs_bin,
        report_path,
        json_out,
        quick,
    })
}

fn utc_timestamp_iso() -> String {
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("epoch_ms:{}", metrics::now_ms()))
}

fn cmd_version(bin: &str, arg: &str) -> String {
    Command::new(bin)
        .arg(arg)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn run() -> anyhow::Result<()> {
    let args = parse_args()?;
    let mut guard = BenchWorkspaceGuard::new();
    let cli = LoomCli::new(args.loom_bin.clone(), 180_000);

    let profile = if args
        .loom_bin
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("release")
    {
        "release".to_string()
    } else {
        "debug".to_string()
    };

    eprintln!("[1/5] Running Metric 1: Viewport Spawn Suite (cold, warm_seq, warm_par)...");
    let (spawn, fixture) = metrics::measure_spawn_suite(&cli, &mut guard, args.quick)?;

    let radar_trials = if args.quick { 25 } else { 105 };
    eprintln!("[2/5] Running Metric 2 (N=5): Radar P99 Suite ({radar_trials} trials)...");
    let (radar_5, _) =
        metrics::measure_radar_and_optional_memory(&cli, &mut guard, 5, radar_trials, false)?;

    eprintln!(
        "[3/5] Running Metric 2 & 3 (N=10): Radar P99 ({radar_trials} trials) + Win32 Idle/Peak Memory..."
    );
    let (radar_10, memory_opt) =
        metrics::measure_radar_and_optional_memory(&cli, &mut guard, 10, radar_trials, true)?;
    let memory_10 = memory_opt.context("memory suite did not return metrics")?;

    eprintln!("[4/5] Running Metric 4 (N=5): Multi-Round Refactor + Seal + Git Push + CAS/Git Storage...");
    let storage_5 = metrics::measure_storage_suite(&cli, &mut guard, 5)?;

    eprintln!("[5/5] Running Metric 4 (N=10): Multi-Round Refactor + Seal + Git Push + CAS/Git Storage...");
    let storage_10 = metrics::measure_storage_suite(&cli, &mut guard, 10)?;

    let (os_version, cpu_name, total_ram_bytes) = win32::query_host_summary();
    let full_report = BenchmarkReport {
        generated_at_utc: utc_timestamp_iso(),
        loom_bin: args.loom_bin.display().to_string(),
        loom_profile: profile,
        os_version,
        cpu_name,
        total_ram_gib: total_ram_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        rustc_version: cmd_version("rustc", "--version"),
        git_version: cmd_version("git", "--version"),
        filesystem_note: report::MANDATORY_NTFS_LINE.to_string(),
        fixture,
        spawn,
        radar_5,
        radar_10,
        memory_10,
        storage_5,
        storage_10,
    };

    let markdown = report::render_markdown_report(&full_report);
    if let Some(ref path) = args.report_path {
        std::fs::write(path, markdown.as_bytes())
            .with_context(|| format!("write report {}", path.display()))?;
        eprintln!("Wrote Markdown report to {}", path.display());
    } else {
        println!("{markdown}");
    }

    if let Some(ref path) = args.json_out {
        let json = serde_json::to_string_pretty(&full_report)?;
        std::fs::write(path, json.as_bytes())
            .with_context(|| format!("write JSON output {}", path.display()))?;
        eprintln!("Wrote JSON output to {}", path.display());
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    run()
}
