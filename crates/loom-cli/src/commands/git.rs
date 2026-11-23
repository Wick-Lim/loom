use crate::client::call_rpc;
use crate::commands::resolve_repo_root;
use loom_core::rpc::{RpcMethod, RpcPayload, ViewportRebaseOutcome};
use std::path::Path;

pub async fn cmd_git_push(
    repo: Option<&Path>,
    remote: Option<String>,
    branch: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let payload = call_rpc(&root, RpcMethod::GitPush { remote, branch }).await?;
    match payload {
        RpcPayload::GitPush(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "Pushed branch `{}` to {} ({})",
                    res.branch,
                    res.remote,
                    res.remote_path.display()
                );
                println!(
                    "  Commits created: {} | Capsules: {} | NegativeEdges synced: {}",
                    res.commits_created,
                    res.pushed_capsule_ids.len(),
                    res.synced_negative_edges
                );
                if let Some(head) = &res.head_commit_oid {
                    println!("  HEAD OID: {head}");
                }
                if let Some(notes_oid) = &res.notes_commit_oid {
                    println!("  Notes ({}): {notes_oid}", res.notes_ref);
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

pub async fn cmd_git_pull(
    repo: Option<&Path>,
    remote: Option<String>,
    branch: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let payload = call_rpc(&root, RpcMethod::GitPull { remote, branch }).await?;
    match payload {
        RpcPayload::GitPull(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "Pulled branch `{}` from {} ({})",
                    res.branch,
                    res.remote,
                    res.remote_path.display()
                );
                println!(
                    "  Commits: {} | Restored capsules: {} | External capsules: {} | NegativeEdges: {} | Trunk updated: {}",
                    res.pulled_commit_oids.len(),
                    res.restored_capsule_ids.len(),
                    res.synthesized_capsule_ids.len(),
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
