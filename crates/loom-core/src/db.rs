use crate::model::{
    AstPatchRecord, Blake3Hash, CausalCapsule, GitMapEntry, NegativeEdge, OpKind, OpLogEntry,
    RepoMeta, SymbolConsumeEntry, SymbolProvideEntry, ViewportRecord, ViewportState,
};
use crate::wal::{InverseEffect, StateEffect};
use redb::{
    Database, ReadTransaction, ReadableDatabase, ReadableTable, ReadableTableMetadata, Table,
    TableDefinition, WriteTransaction,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub const REPO_META_KEY: u64 = 0;

pub const TABLE_VIEWPORTS: TableDefinition<'static, &str, &[u8]> =
    TableDefinition::new("viewports");
pub const TABLE_PATCHES: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("patches");
pub const TABLE_CAPSULES: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("capsules");
pub const TABLE_SYMBOL_PROVIDES: TableDefinition<'static, (&str, &str), &[u8]> =
    TableDefinition::new("symbol_provides");
pub const TABLE_SYMBOL_CONSUMES: TableDefinition<'static, (&str, &str), &[u8]> =
    TableDefinition::new("symbol_consumes");
pub const TABLE_NEGATIVE_INDEX: TableDefinition<'static, (&str, u64), &[u8]> =
    TableDefinition::new("negative_index");
pub const TABLE_OP_LOG: TableDefinition<'static, u64, &[u8]> = TableDefinition::new("op_log");
pub const TABLE_GIT_MAP: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("git_map");

pub fn encode_value<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(rmp_serde::to_vec_named(value)?)
}

pub fn decode_value<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<T> {
    Ok(rmp_serde::from_slice(bytes)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DbStats {
    pub viewports: u64,
    pub patches: u64,
    pub capsules: u64,
    pub symbol_provides: u64,
    pub symbol_consumes: u64,
    pub negative_edges: u64,
    pub op_count: u64,
    pub git_maps: u64,
}

pub struct StateDb {
    db: Database,
}

impl StateDb {
    pub fn open_or_create(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let wtx = db.begin_write()?;
        {
            let _ = wtx.open_table(TABLE_VIEWPORTS)?;
            let _ = wtx.open_table(TABLE_PATCHES)?;
            let _ = wtx.open_table(TABLE_CAPSULES)?;
            let _ = wtx.open_table(TABLE_SYMBOL_PROVIDES)?;
            let _ = wtx.open_table(TABLE_SYMBOL_CONSUMES)?;
            let _ = wtx.open_table(TABLE_NEGATIVE_INDEX)?;
            let _ = wtx.open_table(TABLE_OP_LOG)?;
            let _ = wtx.open_table(TABLE_GIT_MAP)?;
        }
        wtx.commit()?;
        Ok(Self { db })
    }

    pub fn begin_write(&self) -> anyhow::Result<WriteTransaction> {
        Ok(self.db.begin_write()?)
    }

    pub fn begin_read(&self) -> anyhow::Result<ReadTransaction> {
        Ok(self.db.begin_read()?)
    }

    // ------------------------------------------------------------------------
    // RepoMeta (stored at op_log[0])
    // ------------------------------------------------------------------------

    pub fn get_repo_meta(&self) -> anyhow::Result<Option<RepoMeta>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_OP_LOG)?;
        match table.get(REPO_META_KEY)? {
            Some(guard) => Ok(Some(decode_value(guard.value())?)),
            None => Ok(None),
        }
    }

    pub fn get_repo_meta_in_tx<T: ReadableTable<u64, &'static [u8]>>(
        table: &T,
    ) -> anyhow::Result<Option<RepoMeta>> {
        match table.get(REPO_META_KEY)? {
            Some(guard) => Ok(Some(decode_value(guard.value())?)),
            None => Ok(None),
        }
    }

    pub fn set_repo_meta_in_tx(
        table: &mut Table<'_, u64, &'static [u8]>,
        meta: &RepoMeta,
    ) -> anyhow::Result<()> {
        let encoded = encode_value(meta)?;
        table.insert(REPO_META_KEY, encoded.as_slice())?;
        Ok(())
    }

    pub fn init_repo_meta(&self, meta: &RepoMeta) -> anyhow::Result<RepoMeta> {
        let wtx = self.begin_write()?;
        let result = {
            let mut table = wtx.open_table(TABLE_OP_LOG)?;
            if let Some(existing) = Self::get_repo_meta_in_tx(&table)? {
                existing
            } else {
                Self::set_repo_meta_in_tx(&mut table, meta)?;
                meta.clone()
            }
        };
        wtx.commit()?;
        Ok(result)
    }

    pub fn update_repo_meta<F>(&self, f: F) -> anyhow::Result<RepoMeta>
    where
        F: FnOnce(&mut RepoMeta) -> anyhow::Result<()>,
    {
        let wtx = self.begin_write()?;
        let updated = {
            let mut table = wtx.open_table(TABLE_OP_LOG)?;
            let mut meta = Self::get_repo_meta_in_tx(&table)?
                .ok_or_else(|| anyhow::anyhow!("repository metadata not initialized"))?;
            f(&mut meta)?;
            Self::set_repo_meta_in_tx(&mut table, &meta)?;
            meta
        };
        wtx.commit()?;
        Ok(updated)
    }

    // ------------------------------------------------------------------------
    // Viewports
    // ------------------------------------------------------------------------

    pub fn list_viewports(&self, include_gc: bool) -> anyhow::Result<Vec<ViewportRecord>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_VIEWPORTS)?;
        let mut out = Vec::new();
        for item in table.iter()? {
            let (_, v) = item?;
            let vp: ViewportRecord = decode_value(v.value())?;
            if include_gc || vp.state != crate::model::ViewportState::GcPending {
                out.push(vp);
            }
        }
        Ok(out)
    }

    pub fn get_viewport(&self, viewport_id: &str) -> anyhow::Result<Option<ViewportRecord>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_VIEWPORTS)?;
        match table.get(viewport_id)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn find_viewport_by_name(&self, name: &str) -> anyhow::Result<Option<ViewportRecord>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_VIEWPORTS)?;
        let mut best: Option<ViewportRecord> = None;
        for item in table.iter()? {
            let (_, v) = item?;
            let vp: ViewportRecord = decode_value(v.value())?;
            if vp.name != name {
                continue;
            }
            let rank = viewport_lookup_rank(vp.state);
            let replace = match &best {
                None => true,
                Some(cur) => {
                    let cur_rank = viewport_lookup_rank(cur.state);
                    rank > cur_rank
                        || (rank == cur_rank
                            && (vp.created_at_ns, vp.id.as_str())
                                > (cur.created_at_ns, cur.id.as_str()))
                }
            };
            if replace {
                best = Some(vp);
            }
        }
        Ok(best)
    }

    pub fn find_active_viewport_by_name_in_tx<T: ReadableTable<&'static str, &'static [u8]>>(
        table: &T,
        name: &str,
    ) -> anyhow::Result<Option<ViewportRecord>> {
        for item in table.iter()? {
            let (_, v) = item?;
            let vp: ViewportRecord = decode_value(v.value())?;
            if vp.name == name && vp.state.is_active() {
                return Ok(Some(vp));
            }
        }
        Ok(None)
    }

    pub fn find_active_viewport_by_name(
        &self,
        name: &str,
    ) -> anyhow::Result<Option<ViewportRecord>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_VIEWPORTS)?;
        Self::find_active_viewport_by_name_in_tx(&table, name)
    }

    pub fn resolve_viewport(&self, id_or_name: &str) -> anyhow::Result<Option<ViewportRecord>> {
        if let Some(vp) = self.get_viewport(id_or_name)? {
            return Ok(Some(vp));
        }
        if let Some(vp) = self.find_active_viewport_by_name(id_or_name)? {
            return Ok(Some(vp));
        }
        self.find_viewport_by_name(id_or_name)
    }

    pub fn list_viewports_by_state(
        &self,
        states: &[ViewportState],
    ) -> anyhow::Result<Vec<ViewportRecord>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_VIEWPORTS)?;
        let mut out = Vec::new();
        for item in table.iter()? {
            let (_, v) = item?;
            let vp: ViewportRecord = decode_value(v.value())?;
            if states.contains(&vp.state) {
                out.push(vp);
            }
        }
        Ok(out)
    }

    pub fn transition_viewport_state_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        viewport_id: &str,
        next: ViewportState,
        updated_at_ns: u64,
    ) -> anyhow::Result<ViewportRecord> {
        let raw = table
            .get(viewport_id)?
            .ok_or_else(|| anyhow::anyhow!("viewport `{viewport_id}` not found"))?
            .value()
            .to_vec();
        let mut vp: ViewportRecord = decode_value(&raw)?;
        anyhow::ensure!(
            vp.state.can_transition_to(next),
            "invalid viewport state transition: {} -> {}",
            vp.state,
            next
        );
        vp.state = next;
        vp.updated_at_ns = updated_at_ns;
        Self::upsert_viewport_in_tx(table, &vp)?;
        Ok(vp)
    }

    pub fn count_active_viewports(&self) -> anyhow::Result<usize> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_VIEWPORTS)?;
        let mut count = 0usize;
        for item in table.iter()? {
            let (_, v) = item?;
            let vp: ViewportRecord = decode_value(v.value())?;
            if vp.state.is_active() {
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn upsert_viewport_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        vp: &ViewportRecord,
    ) -> anyhow::Result<()> {
        let encoded = encode_value(vp)?;
        table.insert(vp.id.as_str(), encoded.as_slice())?;
        Ok(())
    }

    pub fn upsert_viewport(&self, vp: &ViewportRecord) -> anyhow::Result<()> {
        let wtx = self.begin_write()?;
        {
            let mut table = wtx.open_table(TABLE_VIEWPORTS)?;
            Self::upsert_viewport_in_tx(&mut table, vp)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn delete_viewport_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        viewport_id: &str,
    ) -> anyhow::Result<Option<ViewportRecord>> {
        match table.remove(viewport_id)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn delete_viewport(&self, viewport_id: &str) -> anyhow::Result<Option<ViewportRecord>> {
        let wtx = self.begin_write()?;
        let prev = {
            let mut table = wtx.open_table(TABLE_VIEWPORTS)?;
            Self::delete_viewport_in_tx(&mut table, viewport_id)?
        };
        wtx.commit()?;
        Ok(prev)
    }

    // ------------------------------------------------------------------------
    // OpLog (keys >= 1)
    // ------------------------------------------------------------------------

    pub fn append_op_in_tx(
        table: &mut Table<'_, u64, &'static [u8]>,
        entry: &OpLogEntry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(entry.op_seq >= 1, "op_seq 0 is reserved for RepoMeta");
        let encoded = encode_value(entry)?;
        table.insert(entry.op_seq, encoded.as_slice())?;
        Ok(())
    }

    pub fn append_op(&self, entry: &OpLogEntry) -> anyhow::Result<()> {
        anyhow::ensure!(entry.op_seq >= 1, "op_seq 0 is reserved for RepoMeta");
        let wtx = self.begin_write()?;
        {
            let mut table = wtx.open_table(TABLE_OP_LOG)?;
            Self::append_op_in_tx(&mut table, entry)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_op(&self, op_seq: u64) -> anyhow::Result<Option<OpLogEntry>> {
        if op_seq == 0 {
            return Ok(None);
        }
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_OP_LOG)?;
        match table.get(op_seq)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    fn inverse_has_restore_viewport(inv: &InverseEffect) -> bool {
        match inv {
            InverseEffect::RestoreViewport(_) => true,
            InverseEffect::Batch(items) => items.iter().any(Self::inverse_has_restore_viewport),
            _ => false,
        }
    }

    fn inverse_has_delete_viewport(inv: &InverseEffect) -> bool {
        match inv {
            InverseEffect::DeleteViewport(_) => true,
            InverseEffect::Batch(items) => items.iter().any(Self::inverse_has_delete_viewport),
            _ => false,
        }
    }

    fn db_paths_occupy_same_dir(a: &Path, b: &Path) -> bool {
        if a == b {
            return true;
        }
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(ca), Ok(cb)) => ca == cb,
            _ => false,
        }
    }

    fn inverse_restores_missing_active_dir(inv: &InverseEffect) -> bool {
        match inv {
            InverseEffect::RestoreViewport(vp) => {
                vp.state.is_active() && !vp.path.as_os_str().is_empty() && !vp.path.is_dir()
            }
            InverseEffect::Batch(items) => {
                items.iter().any(Self::inverse_restores_missing_active_dir)
            }
            _ => false,
        }
    }

    fn is_destructive_clean_viewport_op(entry: &OpLogEntry) -> bool {
        match entry.kind {
            OpKind::ViewportDrop => true,
            OpKind::NegativeEdgeRecord => entry
                .inverse_payload
                .as_deref()
                .and_then(|b| decode_value::<InverseEffect>(b).ok())
                .is_some_and(|inv| Self::inverse_has_restore_viewport(&inv)),
            OpKind::ViewportTransition => {
                entry
                    .summary
                    .starts_with("Refresh clean base snapshot for viewport ")
                    && entry
                        .inverse_payload
                        .as_deref()
                        .and_then(|b| decode_value::<InverseEffect>(b).ok())
                        .is_some_and(|inv| Self::inverse_has_restore_viewport(&inv))
            }
            _ => false,
        }
    }

    fn find_later_destructive_clean_for_viewport_in_tx(
        table: &redb::ReadOnlyTable<u64, &'static [u8]>,
        after_op_seq: u64,
        viewport_id: &str,
    ) -> anyhow::Result<Option<OpLogEntry>> {
        for later_item in table.range((after_op_seq.saturating_add(1))..)? {
            let (_, lv) = later_item?;
            let later: OpLogEntry = decode_value(lv.value())?;
            if later.viewport_id.as_deref() == Some(viewport_id)
                && Self::is_destructive_clean_viewport_op(&later)
            {
                return Ok(Some(later));
            }
        }
        Ok(None)
    }

    fn has_later_unundone_git_pull_for_viewport(
        table: &redb::ReadOnlyTable<u64, &'static [u8]>,
        after_op_seq: u64,
        viewport_id: &str,
    ) -> anyhow::Result<bool> {
        for later_item in table.range((after_op_seq.saturating_add(1))..)? {
            let (_, lv) = later_item?;
            let later: OpLogEntry = decode_value(lv.value())?;
            if later.undone_by_op.is_none()
                && matches!(later.kind, OpKind::GitPull | OpKind::RemotePull)
                && later.viewport_id.as_deref() == Some(viewport_id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_later_unundone_vp_op_in_tx(
        table: &redb::ReadOnlyTable<u64, &'static [u8]>,
        after_op_seq: u64,
        viewport_id: &str,
    ) -> anyhow::Result<bool> {
        for later_item in table.range((after_op_seq.saturating_add(1))..)? {
            let (_, lv) = later_item?;
            let later: OpLogEntry = decode_value(lv.value())?;
            if later.undone_by_op.is_none()
                && later.kind != OpKind::Undo
                && (later.inverse_payload.is_some()
                    || matches!(later.kind, OpKind::GitPull | OpKind::RemotePull))
                && later.viewport_id.as_deref() == Some(viewport_id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_later_unundone_trunk_op(
        table: &redb::ReadOnlyTable<u64, &'static [u8]>,
        after_op_seq: u64,
    ) -> anyhow::Result<bool> {
        for item in table.range((after_op_seq.saturating_add(1))..)? {
            let (_, v) = item?;
            let entry: OpLogEntry = decode_value(v.value())?;
            if entry.undone_by_op.is_none()
                && matches!(
                    entry.kind,
                    OpKind::CapsuleSeal
                        | OpKind::GitPull
                        | OpKind::GitPush
                        | OpKind::RemotePull
                        | OpKind::RemotePush
                )
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn latest_undoable_op(&self) -> anyhow::Result<Option<OpLogEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_OP_LOG)?;
        let vp_table = rtx.open_table(TABLE_VIEWPORTS)?;
        let cap_table = rtx.open_table(TABLE_CAPSULES)?;
        let git_map_table = rtx.open_table(TABLE_GIT_MAP)?;
        let mut first_blocked: Option<OpLogEntry> = None;
        for item in table.range(1u64..)?.rev() {
            let (_, v) = item?;
            let entry: OpLogEntry = decode_value(v.value())?;
            if entry.undone_by_op.is_none()
                && entry.kind != OpKind::Undo
                && entry.inverse_payload.is_some()
            {
                if entry.kind == OpKind::CapsuleSeal {
                    let cap_id = entry
                        .capsule_id
                        .as_deref()
                        .or(entry.target_id.as_deref());
                    let is_pushed = if let Some(cid) = cap_id {
                        let pushed_in_cap = cap_table
                            .get(cid)?
                            .and_then(|g| decode_value::<CausalCapsule>(g.value()).ok())
                            .is_some_and(|c| c.git_commit_oid.is_some());
                        let pushed_in_map =
                            git_map_table.get(format!("cap:{cid}").as_str())?.is_some();
                        let pushed_in_remote = git_map_table
                            .get(format!("loom_remote_cap:{cid}").as_str())?
                            .is_some();
                        pushed_in_cap || pushed_in_map || pushed_in_remote
                    } else {
                        false
                    };
                    if is_pushed || Self::has_later_unundone_trunk_op(&table, entry.op_seq)? {
                        continue;
                    }
                } else if entry.kind == OpKind::ViewportDrop {
                    let Some(ref vid) = entry.viewport_id else {
                        continue;
                    };
                    let Some(vp_guard) = vp_table.get(vid.as_str())? else {
                        continue;
                    };
                    let vp: ViewportRecord = decode_value(vp_guard.value())?;
                    if Self::has_later_unundone_git_pull_for_viewport(
                        &table,
                        entry.op_seq,
                        vid.as_str(),
                    )? {
                        continue;
                    }
                    if Self::has_later_unundone_vp_op_in_tx(
                        &table,
                        entry.op_seq,
                        vid.as_str(),
                    )? {
                        continue;
                    }
                    let mut occupied_by_active = false;
                    for other_item in vp_table.iter()? {
                        let (_, ov) = other_item?;
                        let other: ViewportRecord = decode_value(ov.value())?;
                        if other.id != vp.id
                            && other.state.is_active()
                            && Self::db_paths_occupy_same_dir(&other.path, &vp.path)
                        {
                            occupied_by_active = true;
                            break;
                        }
                    }
                    if vp.state != ViewportState::Dropped
                        || vp.pruned_at_ns.is_some()
                        || !vp.path.is_dir()
                        || occupied_by_active
                    {
                        first_blocked.get_or_insert_with(|| entry.clone());
                        continue;
                    }
                } else if let Some(ref vid) = entry.viewport_id
                    && let Some(inv) = entry
                        .inverse_payload
                        .as_deref()
                        .and_then(|b| decode_value::<InverseEffect>(b).ok())
                {
                    if (Self::inverse_has_restore_viewport(&inv)
                        || Self::inverse_has_delete_viewport(&inv))
                        && Self::has_later_unundone_git_pull_for_viewport(
                            &table,
                            entry.op_seq,
                            vid.as_str(),
                        )?
                    {
                        continue;
                    }
                    if Self::inverse_has_restore_viewport(&inv)
                        && Self::find_later_destructive_clean_for_viewport_in_tx(
                            &table,
                            entry.op_seq,
                            vid.as_str(),
                        )?
                        .is_some()
                    {
                        continue;
                    }
                    if let Some(vp_guard) = vp_table.get(vid.as_str())? {
                        let vp: ViewportRecord = decode_value(vp_guard.value())?;
                        if vp.state == ViewportState::Verifying
                            && (Self::inverse_has_restore_viewport(&inv)
                                || Self::inverse_has_delete_viewport(&inv))
                        {
                            first_blocked.get_or_insert_with(|| entry.clone());
                            continue;
                        }
                        if matches!(
                            vp.state,
                            ViewportState::Sealed | ViewportState::Dropped | ViewportState::GcPending
                        ) || vp.pruned_at_ns.is_some()
                        {
                            if Self::inverse_has_restore_viewport(&inv) {
                                continue;
                            }
                            if Self::inverse_has_delete_viewport(&inv)
                                && (matches!(
                                    vp.state,
                                    ViewportState::Sealed | ViewportState::GcPending
                                ) || vp.pruned_at_ns.is_some())
                            {
                                continue;
                            }
                        }
                    }
                    if Self::inverse_has_delete_viewport(&inv)
                        && Self::has_later_unundone_vp_op_in_tx(
                            &table,
                            entry.op_seq,
                            vid.as_str(),
                        )?
                    {
                        continue;
                    }
                    if Self::inverse_restores_missing_active_dir(&inv) {
                        first_blocked.get_or_insert_with(|| entry.clone());
                        continue;
                    }
                }
                return Ok(Some(entry));
            }
        }
        Ok(first_blocked)
    }

    pub fn later_unundone_trunk_op(&self, after_op_seq: u64) -> anyhow::Result<Option<OpLogEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_OP_LOG)?;
        for item in table.range((after_op_seq.saturating_add(1))..)? {
            let (_, v) = item?;
            let entry: OpLogEntry = decode_value(v.value())?;
            if entry.undone_by_op.is_none()
                && matches!(
                    entry.kind,
                    OpKind::CapsuleSeal
                        | OpKind::GitPull
                        | OpKind::GitPush
                        | OpKind::RemotePull
                        | OpKind::RemotePush
                )
            {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    pub fn later_destructive_clean_for_viewport(
        &self,
        after_op_seq: u64,
        viewport_id: &str,
    ) -> anyhow::Result<Option<OpLogEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_OP_LOG)?;
        Self::find_later_destructive_clean_for_viewport_in_tx(&table, after_op_seq, viewport_id)
    }

    pub fn later_unundone_undoable_viewport_op(
        &self,
        after_op_seq: u64,
        viewport_id: &str,
    ) -> anyhow::Result<Option<OpLogEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_OP_LOG)?;
        for item in table.range((after_op_seq.saturating_add(1))..)? {
            let (_, v) = item?;
            let entry: OpLogEntry = decode_value(v.value())?;
            if entry.undone_by_op.is_none()
                && entry.kind != OpKind::Undo
                && (entry.inverse_payload.is_some()
                    || matches!(entry.kind, OpKind::GitPull | OpKind::RemotePull))
                && entry.viewport_id.as_deref() == Some(viewport_id)
            {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    pub fn list_ops(
        &self,
        capsules_only: bool,
        symbol: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<OpLogEntry>> {
        let rtx = self.begin_read()?;
        let op_table = rtx.open_table(TABLE_OP_LOG)?;
        let patch_table = rtx.open_table(TABLE_PATCHES)?;
        let capsule_table = rtx.open_table(TABLE_CAPSULES)?;

        let mut results = Vec::new();
        for item in op_table.range(1u64..)?.rev() {
            let (_, v) = item?;
            let entry: OpLogEntry = decode_value(v.value())?;

            if capsules_only
                && entry.kind != OpKind::CapsuleSeal
                && entry.capsule_id.is_none()
            {
                continue;
            }

            if let Some(sym) = symbol {
                let mut matched = entry
                    .touched_symbols
                    .iter()
                    .any(|s| symbol_matches_query(s, sym));

                if !matched
                    && let Some(cap_id) = entry
                        .capsule_id
                        .as_deref()
                        .or_else(|| {
                            (entry.kind == OpKind::CapsuleSeal)
                                .then_some(entry.target_id.as_deref())
                                .flatten()
                        })
                    && let Some(guard) = capsule_table.get(cap_id)?
                {
                    let cap: CausalCapsule = decode_value(guard.value())?;
                    if cap
                        .touched_symbols
                        .iter()
                        .any(|s| symbol_matches_query(s, sym))
                    {
                        matched = true;
                    }
                }

                if !matched
                    && entry.kind == OpKind::AstPatch
                    && let Some(patch_id) = entry.target_id.as_deref()
                    && let Some(guard) = patch_table.get(patch_id)?
                {
                    let rec: AstPatchRecord = decode_value(guard.value())?;
                    if patch_touches_symbol(&rec, sym) {
                        matched = true;
                    }
                }

                if !matched {
                    continue;
                }
            }

            results.push(entry);
            if let Some(max) = limit
                && results.len() >= max
            {
                break;
            }
        }
        Ok(results)
    }

    pub fn mark_op_undone_in_tx(
        table: &mut Table<'_, u64, &'static [u8]>,
        target_op_seq: u64,
        undone_by_op_seq: u64,
    ) -> anyhow::Result<OpLogEntry> {
        anyhow::ensure!(target_op_seq >= 1, "op_seq 0 is reserved for RepoMeta");
        let existing_bytes = table
            .get(target_op_seq)?
            .ok_or_else(|| anyhow::anyhow!("op_seq {target_op_seq} not found"))?
            .value()
            .to_vec();
        let mut entry: OpLogEntry = decode_value(&existing_bytes)?;
        anyhow::ensure!(
            entry.undone_by_op.is_none(),
            "op_seq {target_op_seq} is already undone"
        );
        entry.undone_by_op = Some(undone_by_op_seq);
        let encoded = encode_value(&entry)?;
        table.insert(target_op_seq, encoded.as_slice())?;
        Ok(entry)
    }

    pub fn mark_op_undone(
        &self,
        target_op_seq: u64,
        undone_by_op_seq: u64,
    ) -> anyhow::Result<OpLogEntry> {
        let wtx = self.begin_write()?;
        let updated = {
            let mut table = wtx.open_table(TABLE_OP_LOG)?;
            Self::mark_op_undone_in_tx(&mut table, target_op_seq, undone_by_op_seq)?
        };
        wtx.commit()?;
        Ok(updated)
    }

    // ------------------------------------------------------------------------
    // Patches
    // ------------------------------------------------------------------------

    pub fn upsert_patch_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        rec: &AstPatchRecord,
    ) -> anyhow::Result<()> {
        let encoded = encode_value(rec)?;
        table.insert(rec.patch_id.as_str(), encoded.as_slice())?;
        Ok(())
    }

    pub fn delete_patch_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        patch_id: &str,
    ) -> anyhow::Result<Option<AstPatchRecord>> {
        match table.remove(patch_id)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn get_patch(&self, patch_id: &str) -> anyhow::Result<Option<AstPatchRecord>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_PATCHES)?;
        match table.get(patch_id)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_patches(&self) -> anyhow::Result<Vec<AstPatchRecord>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_PATCHES)?;
        let mut out = Vec::new();
        for item in table.iter()? {
            let (_, v) = item?;
            out.push(decode_value(v.value())?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------
    // Capsules
    // ------------------------------------------------------------------------

    pub fn upsert_capsule_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        capsule: &CausalCapsule,
    ) -> anyhow::Result<()> {
        let encoded = encode_value(capsule)?;
        table.insert(capsule.capsule_id.as_str(), encoded.as_slice())?;
        Ok(())
    }

    pub fn delete_capsule_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        capsule_id: &str,
    ) -> anyhow::Result<Option<CausalCapsule>> {
        match table.remove(capsule_id)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn get_capsule(&self, capsule_id: &str) -> anyhow::Result<Option<CausalCapsule>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_CAPSULES)?;
        match table.get(capsule_id)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_capsules(&self) -> anyhow::Result<Vec<CausalCapsule>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_CAPSULES)?;
        let mut out = Vec::new();
        for item in table.iter()? {
            let (_, v) = item?;
            out.push(decode_value(v.value())?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------
    // Symbol Provides / Consumes
    // ------------------------------------------------------------------------

    pub fn upsert_symbol_provide_in_tx(
        table: &mut Table<'_, (&'static str, &'static str), &'static [u8]>,
        entry: &SymbolProvideEntry,
    ) -> anyhow::Result<()> {
        let encoded = encode_value(entry)?;
        table.insert(
            (entry.symbol_fqn.as_str(), entry.viewport_id.as_str()),
            encoded.as_slice(),
        )?;
        Ok(())
    }

    pub fn remove_symbol_provide_in_tx(
        table: &mut Table<'_, (&'static str, &'static str), &'static [u8]>,
        symbol_fqn: &str,
        viewport_id: &str,
    ) -> anyhow::Result<Option<SymbolProvideEntry>> {
        match table.remove((symbol_fqn, viewport_id))? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_symbol_provides(
        &self,
        symbol_fqn: Option<&str>,
    ) -> anyhow::Result<Vec<SymbolProvideEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_SYMBOL_PROVIDES)?;
        let mut out = Vec::new();
        if let Some(sym) = symbol_fqn {
            for item in table.range((sym, "")..)? {
                let (k, v) = item?;
                if k.value().0 != sym {
                    break;
                }
                out.push(decode_value(v.value())?);
            }
        } else {
            for item in table.iter()? {
                let (_, v) = item?;
                out.push(decode_value(v.value())?);
            }
        }
        Ok(out)
    }

    pub fn upsert_symbol_consume_in_tx(
        table: &mut Table<'_, (&'static str, &'static str), &'static [u8]>,
        entry: &SymbolConsumeEntry,
    ) -> anyhow::Result<()> {
        let encoded = encode_value(entry)?;
        table.insert(
            (entry.symbol_fqn.as_str(), entry.viewport_id.as_str()),
            encoded.as_slice(),
        )?;
        Ok(())
    }

    pub fn remove_symbol_consume_in_tx(
        table: &mut Table<'_, (&'static str, &'static str), &'static [u8]>,
        symbol_fqn: &str,
        viewport_id: &str,
    ) -> anyhow::Result<Option<SymbolConsumeEntry>> {
        match table.remove((symbol_fqn, viewport_id))? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_symbol_consumes(
        &self,
        symbol_fqn: Option<&str>,
    ) -> anyhow::Result<Vec<SymbolConsumeEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_SYMBOL_CONSUMES)?;
        let mut out = Vec::new();
        if let Some(sym) = symbol_fqn {
            for item in table.range((sym, "")..)? {
                let (k, v) = item?;
                if k.value().0 != sym {
                    break;
                }
                out.push(decode_value(v.value())?);
            }
        } else {
            for item in table.iter()? {
                let (_, v) = item?;
                out.push(decode_value(v.value())?);
            }
        }
        Ok(out)
    }

    pub fn list_symbol_provides_for_viewport(
        &self,
        viewport_id: &str,
    ) -> anyhow::Result<Vec<SymbolProvideEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_SYMBOL_PROVIDES)?;
        let mut out = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            if k.value().1 == viewport_id {
                out.push(decode_value(v.value())?);
            }
        }
        Ok(out)
    }

    pub fn list_symbol_consumes_for_viewport(
        &self,
        viewport_id: &str,
    ) -> anyhow::Result<Vec<SymbolConsumeEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_SYMBOL_CONSUMES)?;
        let mut out = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            if k.value().1 == viewport_id {
                out.push(decode_value(v.value())?);
            }
        }
        Ok(out)
    }

    pub fn clear_viewport_symbols_in_wtx(
        wtx: &WriteTransaction,
        viewport_id: &str,
    ) -> anyhow::Result<()> {
        {
            let mut tbl = wtx.open_table(TABLE_SYMBOL_PROVIDES)?;
            let keys: Vec<(String, String)> = tbl
                .iter()?
                .filter_map(|res| {
                    let (k, _) = res.ok()?;
                    let (sym, vp) = k.value();
                    (vp == viewport_id).then(|| (sym.to_string(), vp.to_string()))
                })
                .collect();
            for (sym, vp) in keys {
                tbl.remove((sym.as_str(), vp.as_str()))?;
            }
        }
        {
            let mut tbl = wtx.open_table(TABLE_SYMBOL_CONSUMES)?;
            let keys: Vec<(String, String)> = tbl
                .iter()?
                .filter_map(|res| {
                    let (k, _) = res.ok()?;
                    let (sym, vp) = k.value();
                    (vp == viewport_id).then(|| (sym.to_string(), vp.to_string()))
                })
                .collect();
            for (sym, vp) in keys {
                tbl.remove((sym.as_str(), vp.as_str()))?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Negative Index
    // ------------------------------------------------------------------------

    pub fn insert_negative_edge_in_tx(
        table: &mut Table<'_, (&'static str, u64), &'static [u8]>,
        symbol_fqn: &str,
        edge: &NegativeEdge,
    ) -> anyhow::Result<()> {
        edge.validate()?;
        let encoded = encode_value(edge)?;
        table.insert((symbol_fqn, edge.edge_seq), encoded.as_slice())?;
        Ok(())
    }

    pub fn remove_negative_edge_in_tx(
        table: &mut Table<'_, (&'static str, u64), &'static [u8]>,
        symbol_fqn: &str,
        edge_seq: u64,
    ) -> anyhow::Result<Option<NegativeEdge>> {
        match table.remove((symbol_fqn, edge_seq))? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn query_negative_edges(&self, symbol_fqn: &str) -> anyhow::Result<Vec<NegativeEdge>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_NEGATIVE_INDEX)?;
        let mut out = Vec::new();
        for item in table.range((symbol_fqn, 0u64)..=(symbol_fqn, u64::MAX))? {
            let (_, v) = item?;
            out.push(decode_value(v.value())?);
        }
        Ok(out)
    }

    pub fn list_all_negative_edges(&self) -> anyhow::Result<Vec<NegativeEdge>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_NEGATIVE_INDEX)?;
        let mut dedup: BTreeMap<u64, NegativeEdge> = BTreeMap::new();
        for item in table.iter()? {
            let (_, v) = item?;
            let edge: NegativeEdge = decode_value(v.value())?;
            dedup.entry(edge.edge_seq).or_insert(edge);
        }
        Ok(dedup.into_values().rev().collect())
    }

    pub fn query_negative_edges_matching(
        &self,
        symbols: &[String],
        files: &[PathBuf],
    ) -> anyhow::Result<Vec<NegativeEdge>> {
        if symbols.is_empty() && files.is_empty() {
            return Ok(Vec::new());
        }
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_NEGATIVE_INDEX)?;
        let mut dedup: BTreeMap<u64, NegativeEdge> = BTreeMap::new();
        for item in table.iter()? {
            let (k, v) = item?;
            let key_sym = k.value().0;
            let edge: NegativeEdge = decode_value(v.value())?;
            let sym_hit = symbols.iter().any(|q| {
                negative_symbol_matches_query(key_sym, q)
                    || edge
                        .touched_symbols
                        .iter()
                        .any(|ts| negative_symbol_matches_query(ts, q))
            });
            let file_hit = files.iter().any(|f| {
                let norm = normalize_rel_for_match(f);
                let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let name = f.file_name().and_then(|s| s.to_str()).unwrap_or("");
                let check = |s: &str| -> bool {
                    let s_norm = s.replace('\\', "/");
                    s_norm == norm
                        || s_norm == format!("file:{norm}")
                        || (!norm.is_empty() && s_norm.ends_with(&format!("/{norm}")))
                        || (!name.is_empty() && s_norm == name)
                        || (!stem.is_empty()
                            && (s_norm == stem
                                || s_norm.starts_with(&format!("{stem}::"))
                                || s_norm.contains(&format!("::{stem}::"))))
                };
                check(key_sym) || edge.touched_symbols.iter().any(|ts| check(ts))
            });
            if sym_hit || file_hit {
                dedup.entry(edge.edge_seq).or_insert(edge);
            }
        }
        Ok(dedup.into_values().rev().collect())
    }

    // ------------------------------------------------------------------------
    // Git Map
    // ------------------------------------------------------------------------

    pub fn upsert_git_map_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        key: &str,
        entry: &GitMapEntry,
    ) -> anyhow::Result<()> {
        let encoded = encode_value(entry)?;
        table.insert(key, encoded.as_slice())?;
        Ok(())
    }

    pub fn remove_git_map_in_tx(
        table: &mut Table<'_, &'static str, &'static [u8]>,
        key: &str,
    ) -> anyhow::Result<Option<GitMapEntry>> {
        match table.remove(key)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn upsert_git_map(&self, key: &str, entry: &GitMapEntry) -> anyhow::Result<()> {
        let wtx = self.begin_write()?;
        {
            let mut table = wtx.open_table(TABLE_GIT_MAP)?;
            Self::upsert_git_map_in_tx(&mut table, key, entry)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_git_map(&self, key: &str) -> anyhow::Result<Option<GitMapEntry>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_GIT_MAP)?;
        match table.get(key)? {
            Some(v) => Ok(Some(decode_value(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_git_maps(&self) -> anyhow::Result<Vec<(String, GitMapEntry)>> {
        let rtx = self.begin_read()?;
        let table = rtx.open_table(TABLE_GIT_MAP)?;
        let mut out = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            out.push((k.value().to_string(), decode_value(v.value())?));
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------
    // CAS Mark-and-Sweep & Stats
    // ------------------------------------------------------------------------

    pub fn collect_referenced_cas_hashes(&self) -> anyhow::Result<HashSet<Blake3Hash>> {
        let rtx = self.begin_read()?;
        let mut referenced = HashSet::new();
        let mut live_vp_ids = HashSet::new();

        let vp_table = rtx.open_table(TABLE_VIEWPORTS)?;
        for item in vp_table.iter()? {
            let (_, v) = item?;
            let vp: ViewportRecord = decode_value(v.value())?;
            if vp.state != crate::model::ViewportState::GcPending {
                live_vp_ids.insert(vp.id);
                if let Some(h) = vp.base_snapshot_cas {
                    referenced.insert(h);
                }
                if let Some(h) = vp.head_cas {
                    referenced.insert(h);
                }
            }
        }

        let patch_table = rtx.open_table(TABLE_PATCHES)?;
        for item in patch_table.iter()? {
            let (_, v) = item?;
            let rec: AstPatchRecord = decode_value(v.value())?;
            if let Some(h) = rec.patch.cas_before {
                referenced.insert(h);
            }
            if let Some(h) = rec.patch.cas_after {
                referenced.insert(h);
            }
            if let Some(manifest) = &rec.patch.binary_manifest {
                referenced.insert(manifest.file_hash);
                for ch in &manifest.chunk_hashes {
                    referenced.insert(*ch);
                }
            }
        }

        let cap_table = rtx.open_table(TABLE_CAPSULES)?;
        for item in cap_table.iter()? {
            let (_, v) = item?;
            let cap: CausalCapsule = decode_value(v.value())?;
            if let Some(h) = cap.snapshot_cas_hash {
                referenced.insert(h);
            }
            if let Some(h) = cap.proof_cas_hash {
                referenced.insert(h);
            }
        }

        let op_table = rtx.open_table(TABLE_OP_LOG)?;
        for item in op_table.range(1u64..)? {
            let (_, v) = item?;
            let entry: OpLogEntry = decode_value(v.value())?;
            if entry.undone_by_op.is_none()
                && let Some(inv_bytes) = entry.inverse_payload.as_deref()
                && let Ok(inv) = decode_value::<InverseEffect>(inv_bytes)
            {
                inv.collect_trunk_snapshot_hashes(&mut referenced);
                if entry.kind != OpKind::CapsuleSeal {
                    let mut snap_manifests = HashSet::new();
                    inv.collect_restore_viewport_cas_hashes(
                        &|id| live_vp_ids.contains(id),
                        &mut referenced,
                        &mut snap_manifests,
                    );
                }
            }
        }

        Ok(referenced)
    }

    pub fn collect_unundone_trunk_snapshot_hashes(&self) -> anyhow::Result<HashSet<Blake3Hash>> {
        let rtx = self.begin_read()?;
        let mut live_vp_ids = HashSet::new();
        let vp_table = rtx.open_table(TABLE_VIEWPORTS)?;
        for item in vp_table.iter()? {
            let (_, v) = item?;
            let vp: ViewportRecord = decode_value(v.value())?;
            if vp.state != crate::model::ViewportState::GcPending {
                live_vp_ids.insert(vp.id);
            }
        }

        let mut manifests = HashSet::new();
        let mut cas_tmp = HashSet::new();
        let op_table = rtx.open_table(TABLE_OP_LOG)?;
        for item in op_table.range(1u64..)? {
            let (_, v) = item?;
            let entry: OpLogEntry = decode_value(v.value())?;
            if entry.undone_by_op.is_none()
                && let Some(inv_bytes) = entry.inverse_payload.as_deref()
                && let Ok(inv) = decode_value::<InverseEffect>(inv_bytes)
            {
                inv.collect_trunk_snapshot_hashes(&mut manifests);
                if entry.kind != OpKind::CapsuleSeal {
                    inv.collect_restore_viewport_cas_hashes(
                        &|id| live_vp_ids.contains(id),
                        &mut cas_tmp,
                        &mut manifests,
                    );
                }
            }
        }
        Ok(manifests)
    }

    pub fn db_stats(&self) -> anyhow::Result<DbStats> {
        let rtx = self.begin_read()?;
        let vp_table = rtx.open_table(TABLE_VIEWPORTS)?;
        let patch_table = rtx.open_table(TABLE_PATCHES)?;
        let cap_table = rtx.open_table(TABLE_CAPSULES)?;
        let sp_table = rtx.open_table(TABLE_SYMBOL_PROVIDES)?;
        let sc_table = rtx.open_table(TABLE_SYMBOL_CONSUMES)?;
        let neg_table = rtx.open_table(TABLE_NEGATIVE_INDEX)?;
        let op_table = rtx.open_table(TABLE_OP_LOG)?;
        let git_table = rtx.open_table(TABLE_GIT_MAP)?;

        let op_raw = op_table.len()?;
        let has_meta = op_table.get(REPO_META_KEY)?.is_some();
        let op_count = if has_meta {
            op_raw.saturating_sub(1)
        } else {
            op_raw
        };

        Ok(DbStats {
            viewports: vp_table.len()?,
            patches: patch_table.len()?,
            capsules: cap_table.len()?,
            symbol_provides: sp_table.len()?,
            symbol_consumes: sc_table.len()?,
            negative_edges: neg_table.len()?,
            op_count,
            git_maps: git_table.len()?,
        })
    }

    // ------------------------------------------------------------------------
    // Effect Application & Inverse Computation Helpers (used by WriterActor & Recovery)
    // ------------------------------------------------------------------------

    pub fn compute_inverse_effect(&self, effect: &StateEffect) -> anyhow::Result<InverseEffect> {
        match effect {
            StateEffect::None => Ok(InverseEffect::None),
            StateEffect::UpsertViewport(vp) => match self.get_viewport(&vp.id)? {
                Some(old) => Ok(InverseEffect::RestoreViewport(old)),
                None => Ok(InverseEffect::DeleteViewport(vp.id.clone())),
            },
            StateEffect::DeleteViewport(id) => match self.get_viewport(id)? {
                Some(old) => Ok(InverseEffect::RestoreViewport(old)),
                None => Ok(InverseEffect::None),
            },
            StateEffect::UpsertPatch(p) => match self.get_patch(&p.patch_id)? {
                Some(old) => Ok(InverseEffect::RestorePatch(old)),
                None => Ok(InverseEffect::DeletePatch(p.patch_id.clone())),
            },
            StateEffect::UpsertCapsule(c) => match self.get_capsule(&c.capsule_id)? {
                Some(old) => Ok(InverseEffect::RestoreCapsule(old)),
                None => Ok(InverseEffect::DeleteCapsule(c.capsule_id.clone())),
            },
            StateEffect::UpsertSymbolProvide(entry) => {
                let rtx = self.begin_read()?;
                let table = rtx.open_table(TABLE_SYMBOL_PROVIDES)?;
                match table.get((entry.symbol_fqn.as_str(), entry.viewport_id.as_str()))? {
                    Some(v) => Ok(InverseEffect::RestoreSymbolProvide(decode_value(v.value())?)),
                    None => Ok(InverseEffect::RemoveSymbolProvide {
                        symbol_fqn: entry.symbol_fqn.clone(),
                        viewport_id: entry.viewport_id.clone(),
                    }),
                }
            }
            StateEffect::RemoveSymbolProvide {
                symbol_fqn,
                viewport_id,
            } => {
                let rtx = self.begin_read()?;
                let table = rtx.open_table(TABLE_SYMBOL_PROVIDES)?;
                match table.get((symbol_fqn.as_str(), viewport_id.as_str()))? {
                    Some(v) => Ok(InverseEffect::RestoreSymbolProvide(decode_value(v.value())?)),
                    None => Ok(InverseEffect::None),
                }
            }
            StateEffect::UpsertSymbolConsume(entry) => {
                let rtx = self.begin_read()?;
                let table = rtx.open_table(TABLE_SYMBOL_CONSUMES)?;
                match table.get((entry.symbol_fqn.as_str(), entry.viewport_id.as_str()))? {
                    Some(v) => Ok(InverseEffect::RestoreSymbolConsume(decode_value(v.value())?)),
                    None => Ok(InverseEffect::RemoveSymbolConsume {
                        symbol_fqn: entry.symbol_fqn.clone(),
                        viewport_id: entry.viewport_id.clone(),
                    }),
                }
            }
            StateEffect::RemoveSymbolConsume {
                symbol_fqn,
                viewport_id,
            } => {
                let rtx = self.begin_read()?;
                let table = rtx.open_table(TABLE_SYMBOL_CONSUMES)?;
                match table.get((symbol_fqn.as_str(), viewport_id.as_str()))? {
                    Some(v) => Ok(InverseEffect::RestoreSymbolConsume(decode_value(v.value())?)),
                    None => Ok(InverseEffect::None),
                }
            }
            StateEffect::InsertNegativeEdge(edge) => {
                let items: Vec<InverseEffect> = edge
                    .touched_symbols
                    .iter()
                    .map(|sym| InverseEffect::DeleteNegativeEdge {
                        symbol_fqn: sym.clone(),
                        edge_seq: edge.edge_seq,
                    })
                    .collect();
                Ok(InverseEffect::Batch(items))
            }
            StateEffect::UpsertGitMap { key, .. } => match self.get_git_map(key)? {
                Some(old) => Ok(InverseEffect::RestoreGitMap {
                    key: key.clone(),
                    entry: old,
                }),
                None => Ok(InverseEffect::DeleteGitMap { key: key.clone() }),
            },
            StateEffect::RecordTrunkSnapshotTransition {
                pre_snapshot_cas,
                post_snapshot_cas,
            } => Ok(InverseEffect::RestoreTrunkSnapshot {
                pre_snapshot_cas: *pre_snapshot_cas,
                post_snapshot_cas: *post_snapshot_cas,
            }),
            StateEffect::Batch(effects) => {
                let mut inverses = Vec::with_capacity(effects.len());
                for eff in effects.iter().rev() {
                    inverses.push(self.compute_inverse_effect(eff)?);
                }
                Ok(InverseEffect::Batch(inverses))
            }
        }
    }

    pub fn apply_state_effect_in_wtx(
        wtx: &WriteTransaction,
        effect: &StateEffect,
    ) -> anyhow::Result<()> {
        match effect {
            StateEffect::None => {}
            StateEffect::UpsertViewport(vp) => {
                let mut t = wtx.open_table(TABLE_VIEWPORTS)?;
                Self::upsert_viewport_in_tx(&mut t, vp)?;
            }
            StateEffect::DeleteViewport(id) => {
                let mut t = wtx.open_table(TABLE_VIEWPORTS)?;
                Self::delete_viewport_in_tx(&mut t, id)?;
            }
            StateEffect::UpsertPatch(p) => {
                let mut t = wtx.open_table(TABLE_PATCHES)?;
                Self::upsert_patch_in_tx(&mut t, p)?;
            }
            StateEffect::UpsertCapsule(c) => {
                let mut t = wtx.open_table(TABLE_CAPSULES)?;
                Self::upsert_capsule_in_tx(&mut t, c)?;
            }
            StateEffect::UpsertSymbolProvide(entry) => {
                let mut t = wtx.open_table(TABLE_SYMBOL_PROVIDES)?;
                Self::upsert_symbol_provide_in_tx(&mut t, entry)?;
            }
            StateEffect::RemoveSymbolProvide {
                symbol_fqn,
                viewport_id,
            } => {
                let mut t = wtx.open_table(TABLE_SYMBOL_PROVIDES)?;
                Self::remove_symbol_provide_in_tx(&mut t, symbol_fqn, viewport_id)?;
            }
            StateEffect::UpsertSymbolConsume(entry) => {
                let mut t = wtx.open_table(TABLE_SYMBOL_CONSUMES)?;
                Self::upsert_symbol_consume_in_tx(&mut t, entry)?;
            }
            StateEffect::RemoveSymbolConsume {
                symbol_fqn,
                viewport_id,
            } => {
                let mut t = wtx.open_table(TABLE_SYMBOL_CONSUMES)?;
                Self::remove_symbol_consume_in_tx(&mut t, symbol_fqn, viewport_id)?;
            }
            StateEffect::InsertNegativeEdge(edge) => {
                let mut t = wtx.open_table(TABLE_NEGATIVE_INDEX)?;
                for sym in &edge.touched_symbols {
                    Self::insert_negative_edge_in_tx(&mut t, sym, edge)?;
                }
            }
            StateEffect::UpsertGitMap { key, entry } => {
                let mut t = wtx.open_table(TABLE_GIT_MAP)?;
                Self::upsert_git_map_in_tx(&mut t, key, entry)?;
            }
            StateEffect::RecordTrunkSnapshotTransition { .. } => {}
            StateEffect::Batch(effects) => {
                for eff in effects {
                    Self::apply_state_effect_in_wtx(wtx, eff)?;
                }
            }
        }
        Ok(())
    }

    pub fn apply_inverse_effect_in_wtx(
        wtx: &WriteTransaction,
        inverse: &InverseEffect,
    ) -> anyhow::Result<()> {
        match inverse {
            InverseEffect::None => {}
            InverseEffect::RestoreViewport(vp) => {
                let mut t = wtx.open_table(TABLE_VIEWPORTS)?;
                Self::upsert_viewport_in_tx(&mut t, vp)?;
            }
            InverseEffect::DeleteViewport(id) => {
                let mut t = wtx.open_table(TABLE_VIEWPORTS)?;
                Self::delete_viewport_in_tx(&mut t, id)?;
            }
            InverseEffect::RestorePatch(p) => {
                let mut t = wtx.open_table(TABLE_PATCHES)?;
                Self::upsert_patch_in_tx(&mut t, p)?;
            }
            InverseEffect::DeletePatch(id) => {
                let mut t = wtx.open_table(TABLE_PATCHES)?;
                Self::delete_patch_in_tx(&mut t, id)?;
            }
            InverseEffect::RestoreCapsule(c) => {
                let mut t = wtx.open_table(TABLE_CAPSULES)?;
                Self::upsert_capsule_in_tx(&mut t, c)?;
            }
            InverseEffect::DeleteCapsule(id) => {
                let mut t = wtx.open_table(TABLE_CAPSULES)?;
                Self::delete_capsule_in_tx(&mut t, id)?;
            }
            InverseEffect::RestoreSymbolProvide(entry) => {
                let mut t = wtx.open_table(TABLE_SYMBOL_PROVIDES)?;
                Self::upsert_symbol_provide_in_tx(&mut t, entry)?;
            }
            InverseEffect::RemoveSymbolProvide {
                symbol_fqn,
                viewport_id,
            } => {
                let mut t = wtx.open_table(TABLE_SYMBOL_PROVIDES)?;
                Self::remove_symbol_provide_in_tx(&mut t, symbol_fqn, viewport_id)?;
            }
            InverseEffect::RestoreSymbolConsume(entry) => {
                let mut t = wtx.open_table(TABLE_SYMBOL_CONSUMES)?;
                Self::upsert_symbol_consume_in_tx(&mut t, entry)?;
            }
            InverseEffect::RemoveSymbolConsume {
                symbol_fqn,
                viewport_id,
            } => {
                let mut t = wtx.open_table(TABLE_SYMBOL_CONSUMES)?;
                Self::remove_symbol_consume_in_tx(&mut t, symbol_fqn, viewport_id)?;
            }
            InverseEffect::DeleteNegativeEdge {
                symbol_fqn,
                edge_seq,
            } => {
                let mut t = wtx.open_table(TABLE_NEGATIVE_INDEX)?;
                Self::remove_negative_edge_in_tx(&mut t, symbol_fqn, *edge_seq)?;
            }
            InverseEffect::RestoreGitMap { key, entry } => {
                let mut t = wtx.open_table(TABLE_GIT_MAP)?;
                Self::upsert_git_map_in_tx(&mut t, key, entry)?;
            }
            InverseEffect::DeleteGitMap { key } => {
                let mut t = wtx.open_table(TABLE_GIT_MAP)?;
                Self::remove_git_map_in_tx(&mut t, key)?;
            }
            InverseEffect::RestoreTrunkSnapshot { .. }
            | InverseEffect::RestoreHeadCapsule(_) => {}
            InverseEffect::Batch(items) => {
                for item in items {
                    Self::apply_inverse_effect_in_wtx(wtx, item)?;
                }
            }
        }
        Ok(())
    }
}

fn normalize_rel_for_match(p: &Path) -> String {
    p.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn split_sig_overload_suffix(s: &str) -> (&str, Option<&str>) {
    let s = s.strip_suffix("#added").unwrap_or(s);
    if let Some((prefix, suffix)) = s.rsplit_once("#sig:")
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        (prefix, Some(suffix))
    } else {
        (s, None)
    }
}

fn strip_leaf_member_modifiers(s: &str) -> std::borrow::Cow<'_, str> {
    let (scope_opt, leaf) = match s.rsplit_once("::") {
        Some((p, l)) => (Some(p), l),
        None => (None, s),
    };
    let mut cur = leaf;
    if let Some(rest) = cur.strip_prefix("static:") {
        cur = rest;
    }
    if let Some(rest) = cur
        .strip_prefix("get:")
        .or_else(|| cur.strip_prefix("set:"))
        .or_else(|| cur.strip_prefix("del:"))
    {
        cur = rest;
    }
    if cur.len() == leaf.len() {
        std::borrow::Cow::Borrowed(s)
    } else if let Some(scope) = scope_opt {
        std::borrow::Cow::Owned(format!("{scope}::{cur}"))
    } else {
        std::borrow::Cow::Borrowed(cur)
    }
}

fn strip_leaf_static_modifier(s: &str) -> std::borrow::Cow<'_, str> {
    let (scope_opt, leaf) = match s.rsplit_once("::") {
        Some((p, l)) => (Some(p), l),
        None => (None, s),
    };
    if let Some(rest) = leaf.strip_prefix("static:") {
        if let Some(scope) = scope_opt {
            std::borrow::Cow::Owned(format!("{scope}::{rest}"))
        } else {
            std::borrow::Cow::Borrowed(rest)
        }
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

fn fqn_suffix_match(a: &str, b: &str) -> bool {
    a == b
        || a.ends_with(&format!("::{b}"))
        || a.ends_with(&format!(".{b}"))
        || b.ends_with(&format!("::{a}"))
        || b.ends_with(&format!(".{a}"))
}

pub fn is_unqualified_bare_symbol(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && !s.contains("::")
        && !s.contains('.')
        && !s.contains(':')
        && !s.contains('/')
        && !s.contains('\\')
}

fn negative_symbol_matches_query(stored_sym: &str, query: &str) -> bool {
    let s = stored_sym.trim();
    let q = query.trim();
    if s.is_empty() || q.is_empty() {
        return false;
    }
    if s.starts_with("file:") || q.starts_with("file:") {
        return s == q;
    }
    if symbol_matches_query(s, q) {
        return true;
    }
    if is_unqualified_bare_symbol(s) && symbol_matches_query(q, s) {
        return true;
    }
    !s.contains("impl<") && q.contains("impl<") && symbol_matches_query(q, s)
}

pub fn symbol_matches_query(candidate: &str, query: &str) -> bool {
    let c = candidate.trim();
    let q = query.trim();
    if c.is_empty() || q.is_empty() {
        return false;
    }
    if c.starts_with("file:") || q.starts_with("file:") {
        return c == q;
    }
    if fqn_suffix_match(c, q) {
        return true;
    }
    let (c_stripped, c_sig) = split_sig_overload_suffix(c);
    let (q_stripped, q_sig) = split_sig_overload_suffix(q);
    if let (Some(cs), Some(qs)) = (c_sig, q_sig)
        && cs != qs
    {
        return false;
    }
    // Match overload `#sig:N` (or `#added`) against bare/same-overload name
    if (c_stripped != c || q_stripped != q) && fqn_suffix_match(c_stripped, q_stripped) {
        return true;
    }
    let c_mod_stripped = strip_leaf_member_modifiers(c_stripped);
    let q_mod_stripped = strip_leaf_member_modifiers(q_stripped);
    if q_stripped == q_mod_stripped.as_ref()
        && c_stripped != c_mod_stripped.as_ref()
        && fqn_suffix_match(c_mod_stripped.as_ref(), q_stripped)
    {
        return true;
    }
    let c_no_static = strip_leaf_static_modifier(c_stripped);
    let q_no_static = strip_leaf_static_modifier(q_stripped);
    if q_stripped == q_no_static.as_ref()
        && c_stripped != c_no_static.as_ref()
        && fqn_suffix_match(c_no_static.as_ref(), q_stripped)
    {
        return true;
    }
    if !q_stripped.contains("impl<") && c_stripped.contains("impl<") {
        let q_norm = crate::cst::anchor::strip_rust_generic_args(q_stripped);
        let match_cands = |cand_src: &str, target_q: &str| -> bool {
            crate::cst::anchor::expand_rust_impl_fqn_candidates(cand_src)
                .into_iter()
                .any(|cand| {
                    fqn_suffix_match(&cand, target_q)
                        || (!q_norm.is_empty() && fqn_suffix_match(&cand, &q_norm))
                })
        };
        if match_cands(c_stripped, q_stripped) {
            return true;
        }
        if q_stripped == q_mod_stripped.as_ref()
            && c_stripped != c_mod_stripped.as_ref()
            && match_cands(c_mod_stripped.as_ref(), q_stripped)
        {
            return true;
        }
        if q_stripped == q_no_static.as_ref()
            && c_stripped != c_no_static.as_ref()
            && match_cands(c_no_static.as_ref(), q_stripped)
        {
            return true;
        }
    }
    false
}

fn viewport_lookup_rank(state: ViewportState) -> u8 {
    if state.is_active() {
        2
    } else if state != ViewportState::GcPending {
        1
    } else {
        0
    }
}

fn patch_touches_symbol(rec: &AstPatchRecord, sym: &str) -> bool {
    use crate::model::{SemanticOp, SymbolContractDelta};
    if rec.patch.contract_deltas.iter().any(|d| match d {
        SymbolContractDelta::AddedSymbol { symbol_fqn, .. }
        | SymbolContractDelta::RemovedSymbol { symbol_fqn, .. }
        | SymbolContractDelta::SignatureChanged { symbol_fqn, .. }
        | SymbolContractDelta::VisibilityReduced { symbol_fqn, .. } => {
            symbol_matches_query(symbol_fqn, sym)
        }
    }) {
        return true;
    }
    rec.patch.ops.iter().any(|op| match op {
        SemanticOp::InsertMember {
            parent_anchor,
            member_anchor,
            ..
        } => {
            symbol_matches_query(&parent_anchor.symbol_fqn, sym)
                || symbol_matches_query(&member_anchor.symbol_fqn, sym)
        }
        SemanticOp::ReplaceBody { target, .. }
        | SemanticOp::MutateSignature { target, .. }
        | SemanticOp::RemoveNode { target, .. } => symbol_matches_query(&target.symbol_fqn, sym),
        SemanticOp::RenameOrMoveFile { .. } | SemanticOp::TextCrdtFallback { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CowBackendKind, FailureKind, ViewportState, now_ns};
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn init_all_eight_tables_readable_immediately() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        let rtx = db.begin_read().unwrap();
        assert_eq!(rtx.open_table(TABLE_VIEWPORTS).unwrap().len().unwrap(), 0);
        assert_eq!(rtx.open_table(TABLE_PATCHES).unwrap().len().unwrap(), 0);
        assert_eq!(rtx.open_table(TABLE_CAPSULES).unwrap().len().unwrap(), 0);
        assert_eq!(
            rtx.open_table(TABLE_SYMBOL_PROVIDES).unwrap().len().unwrap(),
            0
        );
        assert_eq!(
            rtx.open_table(TABLE_SYMBOL_CONSUMES).unwrap().len().unwrap(),
            0
        );
        assert_eq!(
            rtx.open_table(TABLE_NEGATIVE_INDEX).unwrap().len().unwrap(),
            0
        );
        assert_eq!(rtx.open_table(TABLE_OP_LOG).unwrap().len().unwrap(), 0);
        assert_eq!(rtx.open_table(TABLE_GIT_MAP).unwrap().len().unwrap(), 0);
    }

    #[test]
    fn op_log_zero_reserved_for_repo_meta() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        let meta = RepoMeta {
            repo_id: "repo-1".into(),
            ..RepoMeta::default()
        };
        db.init_repo_meta(&meta).unwrap();

        let bad_op = OpLogEntry {
            op_seq: 0,
            summary: "bad".into(),
            ..OpLogEntry::default()
        };
        assert!(db.append_op(&bad_op).is_err());

        for seq in 1..=3 {
            db.append_op(&OpLogEntry {
                op_seq: seq,
                lsn: seq,
                kind: OpKind::ViewportSpawn,
                summary: format!("op-{seq}"),
                timestamp_ns: now_ns(),
                ..OpLogEntry::default()
            })
            .unwrap();
        }

        let ops = db.list_ops(false, None, None).unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].op_seq, 3);
        assert_eq!(ops[2].op_seq, 1);
        assert_eq!(db.get_repo_meta().unwrap().unwrap().repo_id, "repo-1");
    }

    #[test]
    fn list_ops_filters_capsules_symbol_limit() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        db.init_repo_meta(&RepoMeta::default()).unwrap();

        let wtx = db.begin_write().unwrap();
        {
            let mut caps = wtx.open_table(TABLE_CAPSULES).unwrap();
            StateDb::upsert_capsule_in_tx(
                &mut caps,
                &CausalCapsule {
                    capsule_id: "cap-1".into(),
                    touched_symbols: vec!["crate::alpha".into()],
                    ..CausalCapsule::default()
                },
            )
            .unwrap();
        }
        wtx.commit().unwrap();

        db.append_op(&OpLogEntry {
            op_seq: 1,
            lsn: 1,
            kind: OpKind::ViewportSpawn,
            touched_symbols: vec!["crate::beta".into()],
            summary: "spawn".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        db.append_op(&OpLogEntry {
            op_seq: 2,
            lsn: 2,
            kind: OpKind::CapsuleSeal,
            target_id: Some("cap-1".into()),
            capsule_id: Some("cap-1".into()),
            touched_symbols: vec![],
            summary: "seal cap-1".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        db.append_op(&OpLogEntry {
            op_seq: 3,
            lsn: 3,
            kind: OpKind::CapsuleSeal,
            target_id: Some("cap-2".into()),
            capsule_id: Some("cap-2".into()),
            touched_symbols: vec!["crate::alpha".into()],
            summary: "seal cap-2".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        let cap_only = db.list_ops(true, None, None).unwrap();
        assert_eq!(cap_only.len(), 2);

        let sym_alpha = db.list_ops(true, Some("crate::alpha"), None).unwrap();
        assert_eq!(sym_alpha.len(), 2);

        let limited = db.list_ops(true, Some("crate::alpha"), Some(1)).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].op_seq, 3);
    }

    #[test]
    fn symbol_and_negative_index_range_queries_and_cas_gc_collection() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        let h_keep = Blake3Hash::from_bytes(b"keep");
        let h_gc = Blake3Hash::from_bytes(b"gc");

        let wtx = db.begin_write().unwrap();
        {
            let mut vps = wtx.open_table(TABLE_VIEWPORTS).unwrap();
            StateDb::upsert_viewport_in_tx(
                &mut vps,
                &ViewportRecord {
                    id: "vp-active".into(),
                    name: "active".into(),
                    state: ViewportState::Active,
                    driver: CowBackendKind::WindowsNtfsFallback,
                    base_snapshot_cas: Some(h_keep),
                    path: PathBuf::from("vp1"),
                    ..ViewportRecord::default()
                },
            )
            .unwrap();
            StateDb::upsert_viewport_in_tx(
                &mut vps,
                &ViewportRecord {
                    id: "vp-gc".into(),
                    name: "gc".into(),
                    state: ViewportState::GcPending,
                    driver: CowBackendKind::WindowsNtfsFallback,
                    base_snapshot_cas: Some(h_gc),
                    path: PathBuf::from("vp2"),
                    ..ViewportRecord::default()
                },
            )
            .unwrap();

            let mut sp = wtx.open_table(TABLE_SYMBOL_PROVIDES).unwrap();
            StateDb::upsert_symbol_provide_in_tx(
                &mut sp,
                &SymbolProvideEntry {
                    symbol_fqn: "crate::foo".into(),
                    viewport_id: "vp-active".into(),
                    ..SymbolProvideEntry::default()
                },
            )
            .unwrap();
            StateDb::upsert_symbol_provide_in_tx(
                &mut sp,
                &SymbolProvideEntry {
                    symbol_fqn: "crate::foobar".into(),
                    viewport_id: "vp-active".into(),
                    ..SymbolProvideEntry::default()
                },
            )
            .unwrap();

            let mut neg = wtx.open_table(TABLE_NEGATIVE_INDEX).unwrap();
            StateDb::insert_negative_edge_in_tx(
                &mut neg,
                "crate::foo",
                &NegativeEdge {
                    edge_id: "ne-1".into(),
                    edge_seq: 1,
                    failure_kind: FailureKind::CompileError,
                    lesson_summary: "avoid deadlock".into(),
                    ..NegativeEdge::default()
                },
            )
            .unwrap();
        }
        wtx.commit().unwrap();

        let provides = db.list_symbol_provides(Some("crate::foo")).unwrap();
        assert_eq!(provides.len(), 1);
        assert_eq!(provides[0].symbol_fqn, "crate::foo");

        let edges = db.query_negative_edges("crate::foo").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge_id, "ne-1");

        let refs = db.collect_referenced_cas_hashes().unwrap();
        assert!(refs.contains(&h_keep));
        assert!(!refs.contains(&h_gc));
        assert_eq!(db.count_active_viewports().unwrap(), 1);
    }

    #[test]
    fn viewport_lookup_by_active_name_state_and_transition_enforcement() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();

        db.upsert_viewport(&ViewportRecord {
            id: "vp-old".into(),
            name: "work".into(),
            state: ViewportState::GcPending,
            driver: CowBackendKind::WindowsNtfsFallback,
            ..ViewportRecord::default()
        })
        .unwrap();
        assert!(db.find_active_viewport_by_name("work").unwrap().is_none());
        assert_eq!(
            db.resolve_viewport("work").unwrap().unwrap().id,
            "vp-old"
        );

        db.upsert_viewport(&ViewportRecord {
            id: "vp-new".into(),
            name: "work".into(),
            state: ViewportState::Spawned,
            driver: CowBackendKind::WindowsNtfsFallback,
            ..ViewportRecord::default()
        })
        .unwrap();
        assert_eq!(
            db.find_active_viewport_by_name("work").unwrap().unwrap().id,
            "vp-new"
        );
        assert_eq!(
            db.resolve_viewport("work").unwrap().unwrap().id,
            "vp-new"
        );
        assert_eq!(
            db.resolve_viewport("vp-old").unwrap().unwrap().id,
            "vp-old"
        );

        let spawned = db.list_viewports_by_state(&[ViewportState::Spawned]).unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].id, "vp-new");

        let wtx = db.begin_write().unwrap();
        {
            let mut tbl = wtx.open_table(TABLE_VIEWPORTS).unwrap();
            let updated =
                StateDb::transition_viewport_state_in_tx(&mut tbl, "vp-new", ViewportState::Active, 10)
                    .unwrap();
            assert_eq!(updated.state, ViewportState::Active);

            // Invalid direct transition Active -> GcPending must fail
            assert!(
                StateDb::transition_viewport_state_in_tx(
                    &mut tbl,
                    "vp-new",
                    ViewportState::GcPending,
                    20
                )
                .is_err()
            );
        }
        wtx.commit().unwrap();
    }

    #[test]
    fn find_viewport_by_name_prefers_active_then_non_gc_then_newest_created_at() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();

        // Older GcPending with lexicographically smaller ID ("vp-000")
        db.upsert_viewport(&ViewportRecord {
            id: "vp-000".into(),
            name: "work".into(),
            state: ViewportState::GcPending,
            driver: CowBackendKind::WindowsNtfsFallback,
            created_at_ns: 100,
            ..ViewportRecord::default()
        })
        .unwrap();

        // Newer GcPending with lexicographically larger ID ("vp-999")
        db.upsert_viewport(&ViewportRecord {
            id: "vp-999".into(),
            name: "work".into(),
            state: ViewportState::GcPending,
            driver: CowBackendKind::WindowsNtfsFallback,
            created_at_ns: 200,
            ..ViewportRecord::default()
        })
        .unwrap();
        assert_eq!(
            db.find_viewport_by_name("work").unwrap().unwrap().id,
            "vp-999"
        );

        // Sealed viewport ("vp-500") must beat any GcPending record even if GcPending has higher created_at_ns
        db.upsert_viewport(&ViewportRecord {
            id: "vp-500".into(),
            name: "work".into(),
            state: ViewportState::Sealed,
            driver: CowBackendKind::WindowsNtfsFallback,
            created_at_ns: 150,
            ..ViewportRecord::default()
        })
        .unwrap();
        assert_eq!(
            db.resolve_viewport("work").unwrap().unwrap().id,
            "vp-500"
        );

        // Newer Dropped ("vp-600") beats older Sealed ("vp-500")
        db.upsert_viewport(&ViewportRecord {
            id: "vp-600".into(),
            name: "work".into(),
            state: ViewportState::Dropped,
            driver: CowBackendKind::WindowsNtfsFallback,
            created_at_ns: 300,
            ..ViewportRecord::default()
        })
        .unwrap();
        assert_eq!(
            db.resolve_viewport("work").unwrap().unwrap().id,
            "vp-600"
        );

        // Active ("vp-100") beats all non-active records regardless of created_at_ns
        db.upsert_viewport(&ViewportRecord {
            id: "vp-100".into(),
            name: "work".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            created_at_ns: 50,
            ..ViewportRecord::default()
        })
        .unwrap();
        assert_eq!(
            db.resolve_viewport("work").unwrap().unwrap().id,
            "vp-100"
        );
    }

    #[test]
    fn list_ops_capsules_and_symbol_fqn_matching() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();

        let fid = Blake3Hash::from_bytes(b"src/lib.rs");
        let patch = crate::model::AstPatch {
            patch_hash: Blake3Hash::from_bytes(b"p1"),
            viewport_id: "vp-1".into(),
            actor_id: "agy".into(),
            lamport_ts: 1,
            parents: vec![],
            logical_file_id: fid,
            file_path: PathBuf::from("src/lib.rs"),
            ops: vec![],
            contract_deltas: vec![crate::model::SymbolContractDelta::AddedSymbol {
                symbol_fqn: "MyService::handle_request".into(),
                signature_hash: Blake3Hash::from_bytes(b"sig"),
                signature_text: "fn handle_request()".into(),
            }],
            cas_before: None,
            cas_after: None,
            binary_manifest: None,
            created_at_ns: 10,
        };
        let rec = AstPatchRecord {
            patch_id: "patch-1".into(),
            lsn: 2,
            patch,
        };
        let cap = CausalCapsule {
            capsule_id: "cap-1".into(),
            viewport_id: "vp-1".into(),
            agent_id: "agy".into(),
            summary: "add handle_request".into(),
            patch_ids: vec!["patch-1".into()],
            touched_files: vec![PathBuf::from("src/lib.rs")],
            touched_symbols: vec!["MyService::handle_request".into()],
            verification_proof: None,
            parent_capsule_ids: vec![],
            snapshot_cas_hash: None,
            proof_cas_hash: None,
            git_commit_oid: None,
            created_at_ns: 20,
        };

        let wtx = db.begin_write().unwrap();
        StateDb::apply_state_effect_in_wtx(
            &wtx,
            &crate::wal::StateEffect::Batch(vec![
                crate::wal::StateEffect::UpsertPatch(rec),
                crate::wal::StateEffect::UpsertCapsule(cap),
            ]),
        )
        .unwrap();
        {
            let mut op_table = wtx.open_table(TABLE_OP_LOG).unwrap();
            StateDb::append_op_in_tx(
                &mut op_table,
                &OpLogEntry {
                    op_seq: 1,
                    lsn: 2,
                    kind: OpKind::AstPatch,
                    viewport_id: Some("vp-1".into()),
                    target_id: Some("patch-1".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "patch".into(),
                    inverse_payload: None,
                    undone_by_op: None,
                    timestamp_ns: 10,
                },
            )
            .unwrap();
            StateDb::append_op_in_tx(
                &mut op_table,
                &OpLogEntry {
                    op_seq: 2,
                    lsn: 3,
                    kind: OpKind::CapsuleSeal,
                    viewport_id: Some("vp-1".into()),
                    target_id: Some("cap-1".into()),
                    capsule_id: Some("cap-1".into()),
                    touched_symbols: vec!["MyService::handle_request".into()],
                    summary: "seal".into(),
                    inverse_payload: None,
                    undone_by_op: None,
                    timestamp_ns: 20,
                },
            )
            .unwrap();
        }
        wtx.commit().unwrap();

        let all_for_sym = db.list_ops(false, Some("handle_request"), None).unwrap();
        assert_eq!(all_for_sym.len(), 2);
        let caps_for_sym = db.list_ops(true, Some("handle_request"), None).unwrap();
        assert_eq!(caps_for_sym.len(), 1);
        assert_eq!(caps_for_sym[0].op_seq, 2);
    }

    #[test]
    fn query_negative_edges_matching_empty_query_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&tmp.path().join("state.redb")).unwrap();
        let edge = NegativeEdge {
            edge_id: "ne-1".into(),
            edge_seq: 1,
            viewport_id: "vp-1".into(),
            intent: "test".into(),
            touched_symbols: vec!["foo".into()],
            failure_kind: FailureKind::Deadlock,
            lesson_summary: "known deadlock".into(),
            patch_hashes: vec![],
            source: "loom_drop".into(),
            created_at_ns: 100,
        };
        let wtx = db.begin_write().unwrap();
        {
            let mut tbl = wtx.open_table(TABLE_NEGATIVE_INDEX).unwrap();
            StateDb::insert_negative_edge_in_tx(&mut tbl, "foo", &edge).unwrap();
        }
        wtx.commit().unwrap();

        let empty_res = db.query_negative_edges_matching(&[], &[]).unwrap();
        assert!(empty_res.is_empty());
        let sym_res = db
            .query_negative_edges_matching(&["foo".to_string()], &[])
            .unwrap();
        assert_eq!(sym_res.len(), 1);
    }

    #[test]
    fn git_pull_viewport_barrier_blocks_pre_pull_viewport_ops_only_for_rebased_viewport() {
        let tmp = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&tmp.path().join("state.redb")).unwrap();

        let vp1 = ViewportRecord {
            id: "vp-rebased".into(),
            name: "vp1".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            ..ViewportRecord::default()
        };
        let vp2 = ViewportRecord {
            id: "vp-unrebased".into(),
            name: "vp2".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            ..ViewportRecord::default()
        };
        db.upsert_viewport(&vp1).unwrap();
        db.upsert_viewport(&vp2).unwrap();

        let inv_delete_vp1 = encode_value(&InverseEffect::DeleteViewport("vp-rebased".into())).unwrap();
        let inv_restore_vp1 = encode_value(&InverseEffect::RestoreViewport(vp1.clone())).unwrap();
        let inv_restore_vp2 = encode_value(&InverseEffect::RestoreViewport(vp2.clone())).unwrap();

        let wtx = db.begin_write().unwrap();
        {
            let mut op_table = wtx.open_table(TABLE_OP_LOG).unwrap();
            StateDb::append_op_in_tx(
                &mut op_table,
                &OpLogEntry {
                    op_seq: 1,
                    lsn: 1,
                    kind: OpKind::ViewportSpawn,
                    viewport_id: Some("vp-rebased".into()),
                    target_id: Some("vp-rebased".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "spawn vp1".into(),
                    inverse_payload: Some(inv_delete_vp1),
                    undone_by_op: None,
                    timestamp_ns: 10,
                },
            )
            .unwrap();
            StateDb::append_op_in_tx(
                &mut op_table,
                &OpLogEntry {
                    op_seq: 2,
                    lsn: 2,
                    kind: OpKind::ViewportTransition,
                    viewport_id: Some("vp-rebased".into()),
                    target_id: Some("vp-rebased".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "activate vp1".into(),
                    inverse_payload: Some(inv_restore_vp1),
                    undone_by_op: None,
                    timestamp_ns: 20,
                },
            )
            .unwrap();
            StateDb::append_op_in_tx(
                &mut op_table,
                &OpLogEntry {
                    op_seq: 3,
                    lsn: 3,
                    kind: OpKind::ViewportTransition,
                    viewport_id: Some("vp-unrebased".into()),
                    target_id: Some("vp-unrebased".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "activate vp2".into(),
                    inverse_payload: Some(inv_restore_vp2),
                    undone_by_op: None,
                    timestamp_ns: 30,
                },
            )
            .unwrap();
            // Trunk GitPull (viewport_id: None) + per-viewport GitPull for vp-rebased only
            StateDb::append_op_in_tx(
                &mut op_table,
                &OpLogEntry {
                    op_seq: 4,
                    lsn: 4,
                    kind: OpKind::GitPull,
                    viewport_id: None,
                    target_id: Some("deadbeef".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "trunk pull".into(),
                    inverse_payload: None,
                    undone_by_op: None,
                    timestamp_ns: 40,
                },
            )
            .unwrap();
            StateDb::append_op_in_tx(
                &mut op_table,
                &OpLogEntry {
                    op_seq: 5,
                    lsn: 5,
                    kind: OpKind::GitPull,
                    viewport_id: Some("vp-rebased".into()),
                    target_id: Some("vp-rebased".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "rebase vp1".into(),
                    inverse_payload: None,
                    undone_by_op: None,
                    timestamp_ns: 50,
                },
            )
            .unwrap();
        }
        wtx.commit().unwrap();

        // vp-unrebased is NOT blocked by trunk GitPull (viewport_id: None) or vp-rebased's GitPull
        assert!(
            db.later_unundone_undoable_viewport_op(3, "vp-unrebased")
                .unwrap()
                .is_none()
        );
        // vp-rebased op #1 is blocked by op #2 (later ViewportTransition), and op #2 is blocked by op #5 (GitPull)
        assert_eq!(
            db.later_unundone_undoable_viewport_op(1, "vp-rebased")
                .unwrap()
                .unwrap()
                .op_seq,
            2
        );
        assert_eq!(
            db.later_unundone_undoable_viewport_op(2, "vp-rebased")
                .unwrap()
                .unwrap()
                .op_seq,
            5
        );

        // latest_undoable_op returns op #3 (vp-unrebased); if op #3 is undone, it returns None (skips #2 and #1)
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 3);
        let wtx2 = db.begin_write().unwrap();
        {
            let mut op_table = wtx2.open_table(TABLE_OP_LOG).unwrap();
            StateDb::mark_op_undone_in_tx(&mut op_table, 3, 6).unwrap();
        }
        wtx2.commit().unwrap();
        assert!(db.latest_undoable_op().unwrap().is_none());
    }

    #[test]
    fn latest_undoable_op_skips_pushed_and_trunk_barriered_capsule_seal() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        db.init_repo_meta(&RepoMeta::default()).unwrap();

        let wtx = db.begin_write().unwrap();
        {
            let mut vps = wtx.open_table(TABLE_VIEWPORTS).unwrap();
            StateDb::upsert_viewport_in_tx(
                &mut vps,
                &ViewportRecord {
                    id: "vp-peer".into(),
                    name: "vp-peer".into(),
                    state: ViewportState::Active,
                    driver: CowBackendKind::WindowsNtfsFallback,
                    path: PathBuf::from("vp-peer"),
                    ..ViewportRecord::default()
                },
            )
            .unwrap();
            StateDb::upsert_viewport_in_tx(
                &mut vps,
                &ViewportRecord {
                    id: "vp-sealed".into(),
                    name: "vp-sealed".into(),
                    state: ViewportState::Sealed,
                    driver: CowBackendKind::WindowsNtfsFallback,
                    path: PathBuf::from("vp-sealed"),
                    ..ViewportRecord::default()
                },
            )
            .unwrap();
            let mut caps = wtx.open_table(TABLE_CAPSULES).unwrap();
            StateDb::upsert_capsule_in_tx(
                &mut caps,
                &CausalCapsule {
                    capsule_id: "cap-pushed".into(),
                    viewport_id: "vp-sealed".into(),
                    git_commit_oid: Some("deadbeef".into()),
                    ..CausalCapsule::default()
                },
            )
            .unwrap();
            StateDb::upsert_capsule_in_tx(
                &mut caps,
                &CausalCapsule {
                    capsule_id: "cap-unpushed".into(),
                    viewport_id: "vp-sealed".into(),
                    git_commit_oid: None,
                    ..CausalCapsule::default()
                },
            )
            .unwrap();
            StateDb::upsert_capsule_in_tx(
                &mut caps,
                &CausalCapsule {
                    capsule_id: "cap-mapped".into(),
                    viewport_id: "vp-sealed".into(),
                    git_commit_oid: None,
                    ..CausalCapsule::default()
                },
            )
            .unwrap();
            let mut gmap = wtx.open_table(TABLE_GIT_MAP).unwrap();
            StateDb::upsert_git_map_in_tx(
                &mut gmap,
                "cap:cap-mapped",
                &GitMapEntry {
                    git_commit_oid: "cafebabe".into(),
                    capsule_id: Some("cap-mapped".into()),
                    ..GitMapEntry::default()
                },
            )
            .unwrap();
        }
        wtx.commit().unwrap();

        let inv_peer = encode_value(&InverseEffect::DeleteViewport("vp-peer".into())).unwrap();
        let inv_seal = encode_value(&InverseEffect::DeleteCapsule("cap-pushed".into())).unwrap();
        let inv_mapped = encode_value(&InverseEffect::DeleteCapsule("cap-mapped".into())).unwrap();
        let inv_unpushed =
            encode_value(&InverseEffect::DeleteCapsule("cap-unpushed".into())).unwrap();

        // Case 1: Pushed CapsuleSeal (git_commit_oid.is_some()) + GitPush skipped -> reaches peer op #1
        db.append_op(&OpLogEntry {
            op_seq: 1,
            lsn: 1,
            kind: OpKind::ViewportSpawn,
            viewport_id: Some("vp-peer".into()),
            target_id: Some("vp-peer".into()),
            inverse_payload: Some(inv_peer),
            summary: "spawn peer".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 2,
            lsn: 2,
            kind: OpKind::CapsuleSeal,
            viewport_id: Some("vp-sealed".into()),
            target_id: Some("cap-pushed".into()),
            capsule_id: Some("cap-pushed".into()),
            inverse_payload: Some(inv_seal),
            summary: "seal cap-pushed".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 3,
            lsn: 3,
            kind: OpKind::GitPush,
            inverse_payload: None,
            summary: "git push".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        let latest = db
            .latest_undoable_op()
            .unwrap()
            .expect("peer op must be reachable past pushed CapsuleSeal");
        assert_eq!(latest.op_seq, 1);

        // Case 2: CapsuleSeal mapped only in TABLE_GIT_MAP ("cap:cap-mapped") is also skipped
        db.append_op(&OpLogEntry {
            op_seq: 4,
            lsn: 4,
            kind: OpKind::CapsuleSeal,
            viewport_id: Some("vp-sealed".into()),
            target_id: Some("cap-mapped".into()),
            capsule_id: Some("cap-mapped".into()),
            inverse_payload: Some(inv_mapped),
            summary: "seal cap-mapped".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 1);

        // Case 3: Unpushed head CapsuleSeal IS undoable while no later trunk op exists
        db.append_op(&OpLogEntry {
            op_seq: 5,
            lsn: 5,
            kind: OpKind::CapsuleSeal,
            viewport_id: Some("vp-sealed".into()),
            target_id: Some("cap-unpushed".into()),
            capsule_id: Some("cap-unpushed".into()),
            inverse_payload: Some(inv_unpushed),
            summary: "seal cap-unpushed".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 5);

        // Case 4: Later un-undone GitPull blocks unpushed CapsuleSeal #5 -> skips back to peer op #1
        db.append_op(&OpLogEntry {
            op_seq: 6,
            lsn: 6,
            kind: OpKind::GitPull,
            inverse_payload: None,
            summary: "git pull".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 1);
    }

    #[test]
    fn latest_undoable_op_skips_gc_and_verifying_viewport_drop_without_wedging() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        db.init_repo_meta(&RepoMeta::default()).unwrap();

        let active_dir = dir.path().join("vp-active");
        let peer_dir = dir.path().join("vp-peer");
        let dead_dir = dir.path().join("vp-dead");
        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::create_dir_all(&peer_dir).unwrap();
        std::fs::create_dir_all(&dead_dir).unwrap();

        let vp_active_sealed = ViewportRecord {
            id: "vp-active".into(),
            name: "vp-active".into(),
            state: ViewportState::Sealed,
            driver: CowBackendKind::WindowsNtfsFallback,
            path: active_dir.clone(),
            ..ViewportRecord::default()
        };
        let vp_peer = ViewportRecord {
            id: "vp-peer".into(),
            name: "vp-peer".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            path: peer_dir,
            ..ViewportRecord::default()
        };
        let vp_dead_active = ViewportRecord {
            id: "vp-dead".into(),
            name: "vp-dead".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            path: dead_dir.clone(),
            ..ViewportRecord::default()
        };
        let mut vp_dead_gc = vp_dead_active.clone();
        vp_dead_gc.state = ViewportState::GcPending;
        vp_dead_gc.pruned_at_ns = Some(12345);

        db.upsert_viewport(&vp_active_sealed).unwrap();
        db.upsert_viewport(&vp_peer).unwrap();
        db.upsert_viewport(&vp_dead_gc).unwrap();

        let inv_active = encode_value(&InverseEffect::DeleteViewport("vp-active".into())).unwrap();
        let inv_peer = encode_value(&InverseEffect::DeleteViewport("vp-peer".into())).unwrap();
        let inv_drop = encode_value(&InverseEffect::RestoreViewport(vp_dead_active)).unwrap();

        db.append_op(&OpLogEntry {
            op_seq: 1,
            lsn: 1,
            kind: OpKind::ViewportSpawn,
            viewport_id: Some("vp-active".into()),
            target_id: Some("vp-active".into()),
            inverse_payload: Some(inv_active),
            summary: "spawn active".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 2,
            lsn: 2,
            kind: OpKind::ViewportSpawn,
            viewport_id: Some("vp-peer".into()),
            target_id: Some("vp-peer".into()),
            inverse_payload: Some(inv_peer),
            summary: "spawn peer".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 3,
            lsn: 3,
            kind: OpKind::ViewportDrop,
            viewport_id: Some("vp-dead".into()),
            target_id: Some("ne-1".into()),
            inverse_payload: Some(inv_drop),
            summary: "drop dead".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        // 1. GcPending ViewportDrop (#3) is skipped so vp-peer's op (#2) is returned
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 2);

        // 2. Dropped (unpruned, dir exists) ViewportDrop (#3) IS returned
        let mut vp_dead_dropped = vp_dead_gc.clone();
        vp_dead_dropped.state = ViewportState::Dropped;
        vp_dead_dropped.pruned_at_ns = None;
        db.upsert_viewport(&vp_dead_dropped).unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 3);

        // 3. Externally deleted dir ViewportDrop (#3) is skipped -> returns vp-peer (#2)
        std::fs::remove_dir_all(&dead_dir).unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 2);

        // 4. Occupied path also skips ViewportDrop (#3) -> returns vp-peer (#2)
        std::fs::create_dir_all(&dead_dir).unwrap();
        let mut vp_occupier = vp_active_sealed.clone();
        vp_occupier.state = ViewportState::Active;
        vp_occupier.path = dead_dir.clone();
        db.upsert_viewport(&vp_occupier).unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 2);

        // Restore vp-active to Sealed/original path and vp-dead to missing-dir GcPending
        db.upsert_viewport(&vp_active_sealed).unwrap();
        std::fs::remove_dir_all(&dead_dir).unwrap();
        db.upsert_viewport(&vp_dead_gc).unwrap();

        // 5. When vp-peer is Verifying and vp-dead is missing-dir GcPending (no genuinely
        //    undoable ops remain), latest_undoable_op returns Some(first_blocked) (#3)
        let mut vp_peer_verifying = vp_peer.clone();
        vp_peer_verifying.state = ViewportState::Verifying;
        db.upsert_viewport(&vp_peer_verifying).unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 3);

        // 6. If a third vp-active has an earlier undoable op (#1), both vp-dead (GcPending)
        //    and vp-peer (Verifying) are skipped and vp-active's op (#1) is returned
        let mut vp_active_ready = vp_active_sealed;
        vp_active_ready.state = ViewportState::Active;
        vp_active_ready.path = active_dir;
        db.upsert_viewport(&vp_active_ready).unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 1);
    }

    #[test]
    fn latest_undoable_op_skips_missing_active_dir_viewport_without_wedging_on_spawn() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        db.init_repo_meta(&RepoMeta::default()).unwrap();

        let peer_dir = dir.path().join("vp-peer");
        let gone_dir = dir.path().join("vp-gone");
        std::fs::create_dir_all(&peer_dir).unwrap();
        std::fs::create_dir_all(&gone_dir).unwrap();

        let vp_peer = ViewportRecord {
            id: "vp-peer".into(),
            name: "vp-peer".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            path: peer_dir,
            ..ViewportRecord::default()
        };
        let vp_gone = ViewportRecord {
            id: "vp-gone".into(),
            name: "vp-gone".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            path: gone_dir.clone(),
            ..ViewportRecord::default()
        };
        db.upsert_viewport(&vp_peer).unwrap();
        db.upsert_viewport(&vp_gone).unwrap();

        db.append_op(&OpLogEntry {
            op_seq: 1,
            lsn: 1,
            kind: OpKind::ViewportSpawn,
            viewport_id: Some("vp-peer".into()),
            target_id: Some("vp-peer".into()),
            inverse_payload: Some(
                encode_value(&InverseEffect::DeleteViewport("vp-peer".into())).unwrap(),
            ),
            summary: "spawn peer".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 2,
            lsn: 2,
            kind: OpKind::ViewportSpawn,
            viewport_id: Some("vp-gone".into()),
            target_id: Some("vp-gone".into()),
            inverse_payload: Some(
                encode_value(&InverseEffect::DeleteViewport("vp-gone".into())).unwrap(),
            ),
            summary: "spawn gone".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 3,
            lsn: 3,
            kind: OpKind::ViewportTransition,
            viewport_id: Some("vp-gone".into()),
            target_id: Some("vp-gone".into()),
            inverse_payload: Some(
                encode_value(&InverseEffect::RestoreViewport(vp_gone.clone())).unwrap(),
            ),
            summary: "activate gone".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        // Externally delete vp-gone's directory while it remains Active in state.redb
        std::fs::remove_dir_all(&gone_dir).unwrap();

        // Must skip #3 (missing active dir) AND #2 (blocked by later #3), reaching peer op #1
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 1);

        // With no peer op remaining, must return #3 (first_blocked -> InvalidState) rather than #2 (Conflict)
        db.mark_op_undone(1, 99).unwrap();
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 3);
    }

    #[test]
    fn query_negative_edges_matching_disambiguates_modifiers_and_matches_bare_consumer_symbols() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        let wtx = db.begin_write().unwrap();
        {
            let mut tbl = wtx.open_table(TABLE_NEGATIVE_INDEX).unwrap();
            for (seq, (id, sym)) in (1u64..).zip([
                ("edge-static", "User::static:parse"),
                ("edge-instance", "User::parse"),
                ("edge-bare", "parse"),
                ("edge-getter", "Temp::get:c"),
                ("edge-ext", "rs"),
            ]) {
                let edge = NegativeEdge {
                    edge_id: id.into(),
                    edge_seq: seq,
                    viewport_id: "vp-1".into(),
                    intent: "test".into(),
                    touched_symbols: vec![sym.into()],
                    failure_kind: FailureKind::DesignDeadEnd,
                    lesson_summary: format!("lesson for {sym}"),
                    patch_hashes: Vec::new(),
                    source: "loom_drop".into(),
                    created_at_ns: now_ns(),
                };
                StateDb::insert_negative_edge_in_tx(&mut tbl, sym, &edge).unwrap();
            }
        }
        wtx.commit().unwrap();

        let ids = |syms: &[&str]| -> Vec<String> {
            let owned: Vec<String> = syms.iter().map(|s| (*s).to_string()).collect();
            db.query_negative_edges_matching(&owned, &[])
                .unwrap()
                .into_iter()
                .map(|e| e.edge_id)
                .collect()
        };

        let bare_q = ids(&["parse"]);
        assert!(bare_q.contains(&"edge-static".to_string()));
        assert!(bare_q.contains(&"edge-instance".to_string()));
        assert!(bare_q.contains(&"edge-bare".to_string()));
        assert!(!bare_q.contains(&"edge-getter".to_string()));

        // Expanded file-provides query `"User::static:parse"` matches static & bare consumer `"parse"`,
        // but must NOT falsely match scoped instance `"User::parse"`.
        let static_q = ids(&["User::static:parse"]);
        assert!(static_q.contains(&"edge-static".to_string()));
        assert!(static_q.contains(&"edge-bare".to_string()));
        assert!(
            !static_q.contains(&"edge-instance".to_string()),
            "User::static:parse must not match User::parse: {static_q:?}"
        );

        // Explicit setter query `"Temp::set:c"` must NOT match getter `"Temp::get:c"`.
        let setter_q = ids(&["Temp::set:c"]);
        assert!(
            !setter_q.contains(&"edge-getter".to_string()),
            "Temp::set:c must not match Temp::get:c: {setter_q:?}"
        );
        assert_eq!(ids(&["Temp::get:c"]), vec!["edge-getter".to_string()]);
        assert_eq!(ids(&["c"]), vec!["edge-getter".to_string()]);

        // Pseudo `file:` query must not accidentally match bare `"rs"` extension.
        assert!(ids(&["file:src/foo.rs"]).is_empty());
    }

    #[test]
    fn list_ops_and_sig_overload_guard_prevent_false_cross_matches() {
        // 1. Direct `symbol_matches_query` checks for `file:` and `#sig:N` with modifiers
        assert!(!symbol_matches_query("file:config.json", "json"));
        assert!(!symbol_matches_query("file:src/foo.rs", "rs"));
        assert!(!symbol_matches_query("rs", "file:src/foo.rs"));
        assert!(symbol_matches_query("file:config.json", "file:config.json"));
        assert!(symbol_matches_query("file:src/foo.rs", "file:src/foo.rs"));

        assert!(!symbol_matches_query(
            "Parser::static:parse#sig:1",
            "Parser::parse#sig:2"
        ));
        assert!(symbol_matches_query(
            "Parser::static:parse#sig:1",
            "Parser::parse#sig:1"
        ));
        assert!(symbol_matches_query(
            "Parser::static:parse#sig:1",
            "Parser::parse"
        ));
        assert!(symbol_matches_query(
            "Parser::static:parse#sig:1#added",
            "Parser::parse#sig:1"
        ));
        assert!(!symbol_matches_query(
            "Parser::static:parse#sig:1#added",
            "Parser::parse#sig:2"
        ));

        // 2. `list_ops` with `file:` touched_symbols vs short symbol names and vice versa
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        db.init_repo_meta(&RepoMeta::default()).unwrap();

        db.append_op(&OpLogEntry {
            op_seq: 1,
            lsn: 1,
            kind: OpKind::CapsuleSeal,
            touched_symbols: vec!["file:config.json".into(), "file:src/foo.rs".into()],
            summary: "non-code files".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 2,
            lsn: 2,
            kind: OpKind::AstPatch,
            touched_symbols: vec!["rs".into()],
            summary: "symbol named rs".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        assert!(db.list_ops(false, Some("json"), None).unwrap().is_empty());
        let rs_ops = db.list_ops(false, Some("rs"), None).unwrap();
        assert_eq!(rs_ops.len(), 1);
        assert_eq!(rs_ops[0].op_seq, 2);

        let file_foo_ops = db.list_ops(false, Some("file:src/foo.rs"), None).unwrap();
        assert_eq!(file_foo_ops.len(), 1);
        assert_eq!(file_foo_ops[0].op_seq, 1);

        let file_cfg_ops = db
            .list_ops(false, Some("file:config.json"), None)
            .unwrap();
        assert_eq!(file_cfg_ops.len(), 1);
        assert_eq!(file_cfg_ops[0].op_seq, 1);

        // 3. `query_negative_edges_matching` with `#sig:2` must not match `Parser::static:parse#sig:1`
        let wtx = db.begin_write().unwrap();
        {
            let mut tbl = wtx.open_table(TABLE_NEGATIVE_INDEX).unwrap();
            let edge = NegativeEdge {
                edge_id: "edge-static-sig1".into(),
                edge_seq: 1,
                viewport_id: "vp-1".into(),
                intent: "test".into(),
                touched_symbols: vec!["Parser::static:parse#sig:1".into()],
                failure_kind: FailureKind::DesignDeadEnd,
                lesson_summary: "sig 1 deadend".into(),
                patch_hashes: Vec::new(),
                source: "loom_drop".into(),
                created_at_ns: now_ns(),
            };
            StateDb::insert_negative_edge_in_tx(&mut tbl, "Parser::static:parse#sig:1", &edge)
                .unwrap();
        }
        wtx.commit().unwrap();

        assert!(
            db.query_negative_edges_matching(&["Parser::parse#sig:2".into()], &[])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.query_negative_edges_matching(&["Parser::parse#sig:1".into()], &[])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.query_negative_edges_matching(&["Parser::parse".into()], &[])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn symbol_matches_query_normalizes_rust_impl_while_preserving_explicit_impl_disambiguation() {
        assert!(symbol_matches_query("impl<Foo>::new", "Foo::new"));
        assert!(symbol_matches_query("impl<Foo>#2::new", "Foo::new"));
        assert!(symbol_matches_query(
            "inner::impl<mod_a::Foo<T>>#2::new",
            "Foo::new"
        ));
        assert!(symbol_matches_query(
            "inner::impl<mod_a::Foo<T>>#2::new",
            "mod_a::Foo::new"
        ));
        assert!(symbol_matches_query("impl<Foo<T>>::new", "Foo::<i32>::new"));
        assert!(symbol_matches_query("impl<Display for Foo>::fmt", "Foo::fmt"));
        assert!(symbol_matches_query(
            "impl<Display for Foo>::fmt",
            "Display::fmt"
        ));
        assert!(!symbol_matches_query("impl<Foo>::new", "Bar::new"));
        assert!(!symbol_matches_query("impl<Foo>", "Foo"));
        assert!(!symbol_matches_query("impl<Foo>::new", "impl<Foo>#2::new"));
        assert!(!symbol_matches_query("impl<Foo>#2::new", "impl<Foo>::new"));
        assert!(symbol_matches_query("impl<Foo>#2::new", "impl<Foo>#2::new"));

        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        let wtx = db.begin_write().unwrap();
        {
            let mut tbl = wtx.open_table(TABLE_NEGATIVE_INDEX).unwrap();
            let edge = NegativeEdge {
                edge_id: "edge-impl-2".into(),
                edge_seq: 1,
                viewport_id: "vp-1".into(),
                intent: "test impl".into(),
                touched_symbols: vec!["impl<Foo>#2::new".into()],
                failure_kind: FailureKind::DesignDeadEnd,
                lesson_summary: "impl #2 new failed".into(),
                patch_hashes: Vec::new(),
                source: "loom_drop".into(),
                created_at_ns: now_ns(),
            };
            StateDb::insert_negative_edge_in_tx(&mut tbl, "impl<Foo>#2::new", &edge).unwrap();
        }
        wtx.commit().unwrap();

        assert_eq!(
            db.query_negative_edges_matching(&["Foo::new".into()], &[])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.query_negative_edges_matching(&["impl<Foo>#2::new".into()], &[])
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.query_negative_edges_matching(&["impl<Foo>::new".into()], &[])
                .unwrap()
                .is_empty()
        );
        assert!(
            db.query_negative_edges_matching(&["Bar::new".into()], &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn destructive_clean_barrier_blocks_pre_clean_restore_ops_even_when_undone_without_wedging() {
        let dir = TempDir::new().unwrap();
        let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();
        db.init_repo_meta(&RepoMeta::default()).unwrap();

        let vp1_dir = dir.path().join("vp-1");
        let vp2_dir = dir.path().join("vp-2");
        let vp3_dir = dir.path().join("vp-3");
        let vp4_dir = dir.path().join("vp-4");
        let vp_peer_dir = dir.path().join("vp-peer");
        for d in [&vp1_dir, &vp2_dir, &vp3_dir, &vp4_dir, &vp_peer_dir] {
            std::fs::create_dir_all(d).unwrap();
        }

        let s1 = Blake3Hash::from_bytes(b"snap-1");
        let s2 = Blake3Hash::from_bytes(b"snap-2");
        let make_vp = |id: &str, p: &Path| ViewportRecord {
            id: id.into(),
            name: id.into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            path: p.to_path_buf(),
            base_snapshot_cas: Some(s2),
            ..ViewportRecord::default()
        };

        let vp1_active = make_vp("vp-1", &vp1_dir);
        let vp2_active = make_vp("vp-2", &vp2_dir);
        let vp3_active = make_vp("vp-3", &vp3_dir);
        let vp4_active = make_vp("vp-4", &vp4_dir);
        let vp_peer_active = make_vp("vp-peer", &vp_peer_dir);
        let mut vp1_old_s1 = vp1_active.clone();
        vp1_old_s1.base_snapshot_cas = Some(s1);
        let mut vp2_old_s1 = vp2_active.clone();
        vp2_old_s1.base_snapshot_cas = Some(s1);
        let mut vp3_old_s1 = vp3_active.clone();
        vp3_old_s1.base_snapshot_cas = Some(s1);
        vp3_old_s1.patch_ids = vec!["patch-stale".into()];
        let mut vp4_old = vp4_active.clone();
        vp4_old.base_snapshot_cas = Some(s1);

        for v in [
            &vp1_active,
            &vp2_active,
            &vp3_active,
            &vp4_active,
            &vp_peer_active,
        ] {
            db.upsert_viewport(v).unwrap();
        }

        let inv_vp1 = encode_value(&InverseEffect::RestoreViewport(vp1_old_s1)).unwrap();
        let inv_vp2 = encode_value(&InverseEffect::RestoreViewport(vp2_old_s1)).unwrap();
        let inv_vp3 = encode_value(&InverseEffect::RestoreViewport(vp3_old_s1)).unwrap();
        let inv_vp4 = encode_value(&InverseEffect::RestoreViewport(vp4_old)).unwrap();
        let inv_peer = encode_value(&InverseEffect::DeleteViewport("vp-peer".into())).unwrap();
        let inv_non_destructive_ne = encode_value(&InverseEffect::Batch(vec![
            InverseEffect::DeleteNegativeEdge {
                symbol_fqn: "foo".into(),
                edge_seq: 1,
            },
        ]))
        .unwrap();

        // 1. Peer viewport op (#1) before cleaned viewports
        db.append_op(&OpLogEntry {
            op_seq: 1,
            lsn: 1,
            kind: OpKind::ViewportSpawn,
            viewport_id: Some("vp-peer".into()),
            target_id: Some("vp-peer".into()),
            inverse_payload: Some(inv_peer),
            summary: "spawn peer".into(),
            ..OpLogEntry::default()
        })
        .unwrap();

        // 2. vp-1: Activate (#2) + AstPatch (#3) -> undone ViewportDrop (#4, undone_by_op=5)
        db.append_op(&OpLogEntry {
            op_seq: 2,
            lsn: 2,
            kind: OpKind::ViewportTransition,
            viewport_id: Some("vp-1".into()),
            target_id: Some("vp-1".into()),
            inverse_payload: Some(inv_vp1.clone()),
            summary: "Activate viewport `vp-1`".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 3,
            lsn: 3,
            kind: OpKind::AstPatch,
            viewport_id: Some("vp-1".into()),
            target_id: Some("p-1".into()),
            inverse_payload: Some(inv_vp1.clone()),
            summary: "patch vp-1".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 4,
            lsn: 4,
            kind: OpKind::ViewportDrop,
            viewport_id: Some("vp-1".into()),
            target_id: Some("ne-1".into()),
            inverse_payload: Some(inv_vp1),
            summary: "Drop viewport `vp-1`".into(),
            undone_by_op: Some(5),
            ..OpLogEntry::default()
        })
        .unwrap();

        // 3. vp-2: AstPatch (#6) -> undone NegativeEdgeRecord with RestoreViewport (#7, undone_by_op=8)
        db.append_op(&OpLogEntry {
            op_seq: 6,
            lsn: 6,
            kind: OpKind::AstPatch,
            viewport_id: Some("vp-2".into()),
            target_id: Some("p-2".into()),
            inverse_payload: Some(inv_vp2.clone()),
            summary: "patch vp-2".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 7,
            lsn: 7,
            kind: OpKind::NegativeEdgeRecord,
            viewport_id: Some("vp-2".into()),
            target_id: Some("ne-2".into()),
            inverse_payload: Some(inv_vp2),
            summary: "Record deadend (DesignDeadEnd): clean".into(),
            undone_by_op: Some(8),
            ..OpLogEntry::default()
        })
        .unwrap();

        // 4. vp-3: AstPatch (#9) -> undone RestoreViewportClean ViewportTransition (#10, undone_by_op=11)
        db.append_op(&OpLogEntry {
            op_seq: 9,
            lsn: 9,
            kind: OpKind::AstPatch,
            viewport_id: Some("vp-3".into()),
            target_id: Some("p-3".into()),
            inverse_payload: Some(inv_vp3.clone()),
            summary: "patch vp-3".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 10,
            lsn: 10,
            kind: OpKind::ViewportTransition,
            viewport_id: Some("vp-3".into()),
            target_id: Some("vp-3".into()),
            inverse_payload: Some(inv_vp3),
            summary: "Refresh clean base snapshot for viewport `vp-3`".into(),
            undone_by_op: Some(11),
            ..OpLogEntry::default()
        })
        .unwrap();

        // Pre-clean ops on vp-1, vp-2, vp-3 all see their respective undone destructive clean
        assert_eq!(
            db.later_destructive_clean_for_viewport(2, "vp-1")
                .unwrap()
                .unwrap()
                .op_seq,
            4
        );
        assert_eq!(
            db.later_destructive_clean_for_viewport(3, "vp-1")
                .unwrap()
                .unwrap()
                .op_seq,
            4
        );
        assert_eq!(
            db.later_destructive_clean_for_viewport(6, "vp-2")
                .unwrap()
                .unwrap()
                .op_seq,
            7
        );
        assert_eq!(
            db.later_destructive_clean_for_viewport(9, "vp-3")
                .unwrap()
                .unwrap()
                .op_seq,
            10
        );
        assert!(
            db.later_destructive_clean_for_viewport(1, "vp-peer")
                .unwrap()
                .is_none()
        );

        // latest_undoable_op skips #9, #6, #3, #2 and reaches vp-peer (#1) without wedging
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 1);

        // 5. vp-4: AstPatch (#12) -> undone non-destructive NegativeEdgeRecord (#13, undone_by_op=14)
        //    and undone Activate ViewportTransition (#15, undone_by_op=16): neither is a barrier
        db.append_op(&OpLogEntry {
            op_seq: 12,
            lsn: 12,
            kind: OpKind::AstPatch,
            viewport_id: Some("vp-4".into()),
            target_id: Some("p-4".into()),
            inverse_payload: Some(inv_vp4.clone()),
            summary: "patch vp-4".into(),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 13,
            lsn: 13,
            kind: OpKind::NegativeEdgeRecord,
            viewport_id: Some("vp-4".into()),
            target_id: Some("ne-4".into()),
            inverse_payload: Some(inv_non_destructive_ne),
            summary: "Record deadend (Other): keep dirty".into(),
            undone_by_op: Some(14),
            ..OpLogEntry::default()
        })
        .unwrap();
        db.append_op(&OpLogEntry {
            op_seq: 15,
            lsn: 15,
            kind: OpKind::ViewportTransition,
            viewport_id: Some("vp-4".into()),
            target_id: Some("vp-4".into()),
            inverse_payload: Some(inv_vp4),
            summary: "Activate viewport `vp-4`".into(),
            undone_by_op: Some(16),
            ..OpLogEntry::default()
        })
        .unwrap();

        assert!(
            db.later_destructive_clean_for_viewport(12, "vp-4")
                .unwrap()
                .is_none()
        );
        assert_eq!(db.latest_undoable_op().unwrap().unwrap().op_seq, 12);
    }
}
