use crate::client::call_rpc;
use crate::commands::{resolve_repo_root, resolve_target_viewport};
use loom_core::model::{FailureKind, NegativeEdge};
use loom_core::rpc::{RpcMethod, RpcPayload};
use std::path::Path;

pub async fn cmd_drop(
    repo: Option<&Path>,
    viewport: Option<String>,
    reason: String,
    kind: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    if reason.trim().is_empty() {
        anyhow::bail!("--reason cannot be empty");
    }
    NegativeEdge::validate_lesson(&reason)?;
    if let Some(ref k) = kind {
        let trimmed = k.trim();
        if !trimmed.is_empty() {
            trimmed
                .parse::<FailureKind>()
                .map_err(|e| anyhow::anyhow!(e))?;
        }
    }

    let root = resolve_repo_root(repo)?;
    let viewport_id = resolve_target_viewport(&root, viewport).await?;
    let _ = std::env::set_current_dir(&root);
    let payload = call_rpc(
        &root,
        RpcMethod::DropViewport {
            viewport_id,
            reason,
            kind,
        },
    )
    .await?;

    match payload {
        RpcPayload::Drop(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "Dropped viewport `{}` ({}) -> {}",
                    res.viewport.name, res.viewport.id, res.viewport.state
                );
                println!(
                    "  NegativeEdge: {} (seq={}, kind={})",
                    res.negative_edge.edge_id,
                    res.negative_edge.edge_seq,
                    res.negative_edge.failure_kind
                );
                println!(
                    "  Symbols:      {}",
                    res.negative_edge.touched_symbols.join(", ")
                );
                println!("  Lesson:       {}", res.negative_edge.lesson_summary);
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}
