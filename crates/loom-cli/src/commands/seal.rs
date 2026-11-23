use crate::client::call_rpc;
use crate::commands::{resolve_repo_root, resolve_target_viewport};
use loom_core::rpc::{RpcMethod, RpcPayload};
use std::path::Path;

pub async fn cmd_seal(
    repo: Option<&Path>,
    viewport: Option<String>,
    verify: Option<String>,
    summary: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let root = resolve_repo_root(repo)?;
    let viewport_id = resolve_target_viewport(&root, viewport).await?;
    let _ = std::env::set_current_dir(&root);
    let payload = call_rpc(
        &root,
        RpcMethod::SealCapsule {
            viewport_id,
            verify_cmd: verify,
            summary,
        },
    )
    .await?;

    match payload {
        RpcPayload::CapsuleSeal(capsule) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&capsule)?);
            } else {
                println!("Sealed capsule {}", capsule.capsule_id);
                println!("  Viewport: {}", capsule.viewport_id);
                println!("  Summary:  {}", capsule.summary);
                println!(
                    "  Patches:  {} | Files: {} | Symbols: {}",
                    capsule.patch_ids.len(),
                    capsule.touched_files.len(),
                    capsule.touched_symbols.len()
                );
                if let Some(proof) = &capsule.verification_proof {
                    println!(
                        "  Verify:   passed={} exit={} duration={}ms digest={} mac={}",
                        proof.passed,
                        proof.exit_code,
                        proof.duration_ms,
                        proof.output_digest.short_hex(16),
                        proof.proof_hash.short_hex(16)
                    );
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}
