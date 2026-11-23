use crate::client::{call_rpc, discover_repo_root};
use crate::commands::resolve_repo_root;
use loom_core::config::{LoomConfig, RemoteConfig};
use loom_core::remote::parse_remote_repo_url;
use loom_core::rpc::{RpcMethod, RpcPayload, ViewportRebaseOutcome};
use loom_server::{ServerConfig, start_server};
use serde::Serialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
struct RemoteView {
    name: String,
    url: String,
    has_token: bool,
    token_env: Option<String>,
}

fn load_repo_config(repo_root: &Path) -> anyhow::Result<(PathBuf, LoomConfig)> {
    let cfg_path = repo_root.join(".loom").join("config.toml");
    let cfg = if cfg_path.is_file() {
        LoomConfig::load(&cfg_path)?
    } else {
        LoomConfig::default()
    };
    Ok((cfg_path, cfg))
}

fn validate_remote_name(name: &str) -> anyhow::Result<String> {
    let trimmed = name.trim();
    anyhow::ensure!(
        !trimmed.is_empty()
            && !trimmed.chars().any(char::is_whitespace)
            && !trimmed.contains('/')
            && !trimmed.contains('\\'),
        "invalid remote name `{name}`"
    );
    Ok(trimmed.to_string())
}

pub async fn cmd_serve(
    data_dir: PathBuf,
    bind: SocketAddr,
    tokens: Vec<String>,
    no_auth: bool,
    no_auto_create: bool,
) -> anyhow::Result<()> {
    let handle = start_server(ServerConfig {
        bind,
        data_dir,
        tokens,
        allow_unauthenticated: no_auth,
        allow_auto_create: !no_auto_create,
        ..ServerConfig::default()
    })
    .await?;

    eprintln!(
        "loom-server listening on http://{} (auth={})",
        handle.local_addr,
        if no_auth { "disabled" } else { "bearer" }
    );
    tokio::signal::ctrl_c().await?;
    eprintln!("shutting down loom-server...");
    handle.shutdown().await?;
    Ok(())
}

pub fn cmd_remote_add(
    repo: Option<&Path>,
    name: String,
    url: String,
    token: Option<String>,
    token_env: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let name = validate_remote_name(&name)?;
    let parsed = parse_remote_repo_url(&url).map_err(|m| anyhow::anyhow!("{m}"))?;
    let canonical_url = format!("{}/{}", parsed.base_url, parsed.repo_slug);

    let (cfg_path, mut cfg) = load_repo_config(&root)?;
    if cfg.remotes.contains_key(&name) {
        anyhow::bail!("remote `{name}` already exists (use `loom remote set-url` to update)");
    }
    let clean_token = token.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let clean_env = token_env
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    cfg.remotes.insert(
        name.clone(),
        RemoteConfig {
            url: canonical_url.clone(),
            token: clean_token.clone(),
            token_env: clean_env.clone(),
        },
    );
    cfg.save(&cfg_path)?;

    let view = RemoteView {
        name: name.clone(),
        url: canonical_url,
        has_token: clean_token.is_some(),
        token_env: clean_env,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        println!("Added remote `{}` -> {}", view.name, view.url);
    }
    Ok(())
}

pub fn cmd_remote_set_url(
    repo: Option<&Path>,
    name: String,
    url: String,
    json: bool,
) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let name = validate_remote_name(&name)?;
    let parsed = parse_remote_repo_url(&url).map_err(|m| anyhow::anyhow!("{m}"))?;
    let canonical_url = format!("{}/{}", parsed.base_url, parsed.repo_slug);

    let (cfg_path, mut cfg) = load_repo_config(&root)?;
    let entry = cfg
        .remotes
        .get_mut(&name)
        .ok_or_else(|| anyhow::anyhow!("remote `{name}` not found"))?;
    entry.url = canonical_url.clone();
    let view = RemoteView {
        name: name.clone(),
        url: canonical_url,
        has_token: entry.token.is_some(),
        token_env: entry.token_env.clone(),
    };
    cfg.save(&cfg_path)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        println!("Updated remote `{}` -> {}", view.name, view.url);
    }
    Ok(())
}

pub fn cmd_remote_remove(repo: Option<&Path>, name: String, json: bool) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let name = validate_remote_name(&name)?;
    let (cfg_path, mut cfg) = load_repo_config(&root)?;
    if cfg.remotes.remove(&name).is_none() {
        anyhow::bail!("remote `{name}` not found");
    }
    cfg.save(&cfg_path)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "removed": name }))?
        );
    } else {
        println!("Removed remote `{name}`");
    }
    Ok(())
}

pub fn cmd_remote_list(repo: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let (_, cfg) = load_repo_config(&root)?;
    let views: Vec<RemoteView> = cfg
        .remotes
        .into_iter()
        .map(|(name, r)| RemoteView {
            name,
            url: r.url,
            has_token: r.token.is_some(),
            token_env: r.token_env,
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&views)?);
    } else if views.is_empty() {
        println!("(no remotes configured)");
    } else {
        for v in views {
            let auth_desc = if v.has_token {
                "token=configured"
            } else if let Some(ref env_var) = v.token_env {
                env_var.as_str()
            } else {
                "auth=env/none"
            };
            println!("{}\t{}\t({})", v.name, v.url, auth_desc);
        }
    }
    Ok(())
}

pub async fn cmd_remote_push(
    repo: Option<&Path>,
    remote: Option<String>,
    branch: Option<String>,
    token: Option<String>,
    force: bool,
    json: bool,
) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let payload = call_rpc(
        &root,
        RpcMethod::RemotePush {
            remote,
            branch,
            token,
            force,
        },
    )
    .await?;
    match payload {
        RpcPayload::RemotePush(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "Pushed native Loom state `{}` to {} ({})",
                    res.branch, res.remote, res.remote_url
                );
                println!(
                    "  Capsules: {} | Patches: {} | CAS uploaded: {} | NegativeEdges: {}",
                    res.pushed_capsule_ids.len(),
                    res.pushed_patches,
                    res.uploaded_cas_blobs,
                    res.pushed_negative_edges
                );
                if let Some(head) = &res.head_capsule_id {
                    println!("  HEAD capsule: {head}");
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

pub async fn cmd_remote_pull(
    repo: Option<&Path>,
    remote: Option<String>,
    branch: Option<String>,
    token: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let payload = call_rpc(
        &root,
        RpcMethod::RemotePull {
            remote,
            branch,
            token,
        },
    )
    .await?;
    match payload {
        RpcPayload::RemotePull(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "Pulled native Loom state `{}` from {} ({})",
                    res.branch, res.remote, res.remote_url
                );
                println!(
                    "  Capsules: {} | Patches: {} | CAS downloaded: {} | NegativeEdges: {} | Trunk updated: {}",
                    res.pulled_capsule_ids.len(),
                    res.pulled_patches,
                    res.downloaded_cas_blobs,
                    res.restored_negative_edges,
                    res.updated_trunk
                );
                for vp in &res.rebased_viewports {
                    let status = match vp.outcome {
                        ViewportRebaseOutcome::Clean => "clean",
                        ViewportRebaseOutcome::Conflict => "RED_CONFLICT",
                        ViewportRebaseOutcome::Skipped => "skipped",
                    };
                    println!(
                        "  Rebase `{}` ({}): {} (updated_files={}, conflicts={})",
                        vp.viewport_name,
                        vp.viewport_id,
                        status,
                        vp.updated_files,
                        vp.conflicts.len()
                    );
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_clone(
    url: String,
    dest: Option<PathBuf>,
    branch: Option<String>,
    remote_name: String,
    token: Option<String>,
    token_env: Option<String>,
    colocate_git: bool,
    json: bool,
) -> anyhow::Result<()> {
    let parsed = parse_remote_repo_url(&url).map_err(|m| anyhow::anyhow!("{m}"))?;
    let remote_name = validate_remote_name(&remote_name)?;
    let canonical_url = format!("{}/{}", parsed.base_url, parsed.repo_slug);

    let dest_dir = dest.unwrap_or_else(|| PathBuf::from(&parsed.default_dir_name));
    let dest_existed = dest_dir.exists();
    if dest_existed {
        anyhow::ensure!(
            dest_dir.is_dir(),
            "clone destination `{}` exists and is not a directory",
            dest_dir.display()
        );
        let mut entries = std::fs::read_dir(&dest_dir)?;
        anyhow::ensure!(
            entries.next().is_none(),
            "clone destination `{}` already exists and is not an empty directory",
            dest_dir.display()
        );
    }

    let root = discover_repo_root(Some(&dest_dir), true)?;
    let pull_res = async {
        let init_payload = call_rpc(&root, RpcMethod::Init { colocate_git }).await?;
        let RpcPayload::Init(_) = init_payload else {
            anyhow::bail!("unexpected init response during clone");
        };

        let (cfg_path, mut cfg) = load_repo_config(&root)?;
        cfg.remotes.insert(
            remote_name.clone(),
            RemoteConfig {
                url: canonical_url.clone(),
                token: token.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
                token_env: token_env
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            },
        );
        cfg.save(&cfg_path)?;

        let payload = call_rpc(
            &root,
            RpcMethod::RemotePull {
                remote: Some(remote_name.clone()),
                branch,
                token,
            },
        )
        .await?;
        match payload {
            RpcPayload::RemotePull(res) => Ok(res),
            other => anyhow::bail!("unexpected RPC response: {other:?}"),
        }
    }
    .await;

    match pull_res {
        Ok(res) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "repo_root": root,
                        "remote": res.remote,
                        "remote_url": res.remote_url,
                        "branch": res.branch,
                        "head_capsule_id": res.head_capsule_id,
                        "pulled_capsules": res.pulled_capsule_ids.len(),
                        "pulled_patches": res.pulled_patches,
                        "downloaded_cas_blobs": res.downloaded_cas_blobs,
                        "restored_negative_edges": res.restored_negative_edges,
                    }))?
                );
            } else {
                println!(
                    "Cloned native Loom repository `{}` into {}",
                    res.remote_url,
                    root.display()
                );
                println!(
                    "  Branch: {} | Capsules: {} | Patches: {} | CAS blobs: {} | NegativeEdges: {}",
                    res.branch,
                    res.pulled_capsule_ids.len(),
                    res.pulled_patches,
                    res.downloaded_cas_blobs,
                    res.restored_negative_edges
                );
            }
            Ok(())
        }
        Err(err) => {
            let _ = call_rpc(&root, RpcMethod::Shutdown).await;
            let cleanup_target = if !dest_existed {
                root.clone()
            } else {
                root.join(".loom")
            };
            for _ in 0..60 {
                if !cleanup_target.exists() || std::fs::remove_dir_all(&cleanup_target).is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err)
        }
    }
}
