use loom_core::config::LoomConfig;
use loom_core::db::{
    REPO_META_KEY, StateDb, TABLE_GIT_MAP, TABLE_OP_LOG,
};
use loom_core::model::{OpKind, OpLogEntry, RepoMeta, ViewportRecord, ViewportState, now_ns};
use loom_core::wal::{StateEffect, WalLog, WalPayload, WalRecord, WalRecordKind};
use redb::ReadableTable;
use std::path::Path;

pub struct RecoveryOutcome {
    pub db: StateDb,
    pub wal: WalLog,
    pub replayed_count: usize,
    pub truncated_tail_bytes: u64,
    pub last_checkpoint_lsn: u64,
}

pub fn recover(loom_dir: &Path, config: &LoomConfig) -> anyhow::Result<RecoveryOutcome> {
    std::fs::create_dir_all(loom_dir)?;
    let db = StateDb::open_or_create(&loom_dir.join("state.redb"))?;
    let meta_opt = db.get_repo_meta()?;
    let last_applied_lsn = meta_opt.as_ref().map_or(0, |m| m.last_applied_lsn);

    let (mut wal, scan_report) =
        WalLog::open_and_repair(loom_dir.join("wal"), config.wal_segment_max_bytes)?;

    let pending: Vec<WalRecord> = scan_report
        .records
        .into_iter()
        .filter(|r| r.lsn > last_applied_lsn)
        .collect();
    let replayed_count = pending.len();

    if !pending.is_empty() {
        let wtx = db.begin_write()?;
        for record in &pending {
            apply_wal_record_in_wtx(&wtx, record)?;
        }
        wtx.commit()?;

        let latest_meta = db.get_repo_meta()?;
        let ckpt_lsn = wal.last_lsn() + 1;
        let last_op_seq = latest_meta
            .as_ref()
            .map_or(0, |m| m.next_op_seq.saturating_sub(1));
        let now = now_ns();
        wal.append_checkpoint(ckpt_lsn, now, ckpt_lsn, last_op_seq)?;

        if latest_meta.is_some() {
            db.update_repo_meta(|meta| {
                meta.last_applied_lsn = ckpt_lsn;
                meta.last_checkpoint_lsn = ckpt_lsn;
                meta.next_lsn = meta.next_lsn.max(ckpt_lsn + 1);
                meta.updated_at_ns = now;
                Ok(())
            })?;
        }

        let _ = wal.compact_up_to(ckpt_lsn)?;
    }

    // Any viewport left in `Verifying` from a crashed daemon cannot complete its dead
    // verification command; transition it back to `Active` durably so seal/drop/undo/gc
    // and IdleMonitor recover cleanly.
    let verifying_vps: Vec<ViewportRecord> = db
        .list_viewports(true)?
        .into_iter()
        .filter(|vp| vp.state == ViewportState::Verifying)
        .collect();
    if !verifying_vps.is_empty()
        && let Some(mut meta) = db.get_repo_meta()?
    {
        let mut lsn = meta.next_lsn.max(wal.last_lsn() + 1);
        let mut op_seq = meta.next_op_seq.max(1);
        let now = now_ns();
        let mut recovered_records: Vec<(OpLogEntry, StateEffect, u64)> =
            Vec::with_capacity(verifying_vps.len());

        for mut vp in verifying_vps {
            meta.lamport_clock += 1;
            vp.state = ViewportState::Active;
            vp.head_lsn = lsn;
            vp.lamport_ts = meta.lamport_clock;
            vp.updated_at_ns = now;
            let op = OpLogEntry {
                op_seq,
                lsn,
                kind: OpKind::ViewportTransition,
                viewport_id: Some(vp.id.clone()),
                target_id: Some(vp.id.clone()),
                capsule_id: None,
                touched_symbols: Vec::new(),
                summary: format!(
                    "Recover stranded verifying viewport `{}`: Verifying -> Active",
                    vp.name
                ),
                inverse_payload: None,
                undone_by_op: None,
                timestamp_ns: now,
            };
            let effect = StateEffect::UpsertViewport(vp);
            let payload = WalPayload::Op {
                op: op.clone(),
                effect: effect.clone(),
            };
            wal.append(WalRecordKind::Mutation, lsn, now, &payload)?;
            recovered_records.push((op, effect, lsn));
            lsn += 1;
            op_seq += 1;
        }

        if let Some((_, _, last_lsn)) = recovered_records.last() {
            meta.next_lsn = lsn;
            meta.next_op_seq = op_seq;
            meta.last_applied_lsn = meta.last_applied_lsn.max(*last_lsn);
            meta.updated_at_ns = now;
            let wtx = db.begin_write()?;
            for (op, effect, _) in &recovered_records {
                StateDb::apply_state_effect_in_wtx(&wtx, effect)?;
                let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
                StateDb::append_op_in_tx(&mut op_table, op)?;
            }
            {
                let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
                StateDb::set_repo_meta_in_tx(&mut op_table, &meta)?;
            }
            wtx.commit()?;
        }
    }

    let final_ckpt = db
        .get_repo_meta()?
        .map_or(0, |m| m.last_checkpoint_lsn);

    Ok(RecoveryOutcome {
        db,
        wal,
        replayed_count,
        truncated_tail_bytes: scan_report.truncated_tail_bytes,
        last_checkpoint_lsn: final_ckpt,
    })
}

fn apply_wal_record_in_wtx(
    wtx: &redb::WriteTransaction,
    record: &WalRecord,
) -> anyhow::Result<()> {
    match &record.payload {
        WalPayload::InitRepo {
            meta,
            init_op,
            git_seed,
        } => {
            {
                let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
                if StateDb::get_repo_meta_in_tx(&op_table)?.is_none() {
                    let mut m = meta.clone();
                    m.last_applied_lsn = m.last_applied_lsn.max(record.lsn);
                    m.next_lsn = m.next_lsn.max(record.lsn + 1);
                    StateDb::set_repo_meta_in_tx(&mut op_table, &m)?;
                }
                if let Some(op) = init_op
                    && op_table.get(op.op_seq)?.is_none()
                {
                    StateDb::append_op_in_tx(&mut op_table, op)?;
                }
            }
            if let Some((key, entry)) = git_seed {
                let mut git_table = wtx.open_table(TABLE_GIT_MAP)?;
                if git_table.get(key.as_str())?.is_none() {
                    StateDb::upsert_git_map_in_tx(&mut git_table, key, entry)?;
                }
            }
        }
        WalPayload::Op { op, effect } => {
            let already_applied = {
                let op_table = wtx.open_table(TABLE_OP_LOG)?;
                op_table.get(op.op_seq)?.is_some()
            };
            if !already_applied {
                StateDb::apply_state_effect_in_wtx(wtx, effect)?;
                let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
                StateDb::append_op_in_tx(&mut op_table, op)?;
            }
            let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
            let mut meta = StateDb::get_repo_meta_in_tx(&op_table)?
                .unwrap_or_else(RepoMeta::default);
            meta.last_applied_lsn = meta.last_applied_lsn.max(record.lsn);
            meta.next_lsn = meta.next_lsn.max(record.lsn + 1);
            meta.next_op_seq = meta.next_op_seq.max(op.op_seq + 1);
            if !already_applied {
                meta.lamport_clock = meta.lamport_clock.saturating_add(1);
                crate::advance_meta_for_effect(&mut meta, op.kind, effect);
            }
            meta.updated_at_ns = record.timestamp_ns;
            let encoded = loom_core::db::encode_value(&meta)?;
            op_table.insert(REPO_META_KEY, encoded.as_slice())?;
        }
        WalPayload::Undo {
            undo_op,
            target_op_seq,
            inverse,
        } => {
            let already_applied = {
                let op_table = wtx.open_table(TABLE_OP_LOG)?;
                op_table.get(undo_op.op_seq)?.is_some()
            };
            if !already_applied {
                StateDb::apply_inverse_effect_in_wtx(wtx, inverse)?;
                let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
                StateDb::append_op_in_tx(&mut op_table, undo_op)?;
                let target_bytes_opt = op_table
                    .get(*target_op_seq)?
                    .map(|g| g.value().to_vec());
                if let Some(target_bytes) = target_bytes_opt {
                    let mut target_entry: loom_core::model::OpLogEntry =
                        loom_core::db::decode_value(&target_bytes)?;
                    if target_entry.undone_by_op.is_none() {
                        target_entry.undone_by_op = Some(undo_op.op_seq);
                        let enc = loom_core::db::encode_value(&target_entry)?;
                        op_table.insert(*target_op_seq, enc.as_slice())?;
                    }
                }
            }
            let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
            let mut meta = StateDb::get_repo_meta_in_tx(&op_table)?
                .unwrap_or_else(RepoMeta::default);
            meta.last_applied_lsn = meta.last_applied_lsn.max(record.lsn);
            meta.next_lsn = meta.next_lsn.max(record.lsn + 1);
            meta.next_op_seq = meta.next_op_seq.max(undo_op.op_seq + 1);
            if !already_applied {
                meta.lamport_clock = meta.lamport_clock.saturating_add(1);
                crate::apply_inverse_meta_updates(wtx, &mut meta, inverse)?;
            }
            meta.updated_at_ns = record.timestamp_ns;
            StateDb::set_repo_meta_in_tx(&mut op_table, &meta)?;
        }
        WalPayload::Checkpoint {
            checkpoint_lsn,
            last_applied_lsn,
            last_op_seq,
        } => {
            let mut op_table = wtx.open_table(TABLE_OP_LOG)?;
            if let Some(mut meta) = StateDb::get_repo_meta_in_tx(&op_table)? {
                meta.last_applied_lsn = meta
                    .last_applied_lsn
                    .max(*last_applied_lsn)
                    .max(record.lsn);
                meta.last_checkpoint_lsn = meta.last_checkpoint_lsn.max(*checkpoint_lsn);
                meta.next_lsn = meta.next_lsn.max(record.lsn + 1);
                meta.next_op_seq = meta.next_op_seq.max(last_op_seq + 1);
                StateDb::set_repo_meta_in_tx(&mut op_table, &meta)?;
            }
        }
    }
    Ok(())
}
