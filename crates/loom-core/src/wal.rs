use crate::model::{
    AstPatchRecord, Blake3Hash, CausalCapsule, GitMapEntry, NegativeEdge, OpLogEntry, RepoMeta,
    SymbolConsumeEntry, SymbolProvideEntry, ViewportRecord,
};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use thiserror::Error;

pub const WAL_MAGIC: [u8; 4] = *b"LMWA";
pub const WAL_VERSION: u16 = 1;
pub const WAL_HEADER_LEN: usize = 28;
pub const WAL_MAC_LEN: usize = 32;
pub const WAL_FRAME_OVERHEAD: usize = WAL_HEADER_LEN + WAL_MAC_LEN; // 60 bytes
pub const MAX_WAL_PAYLOAD_LEN: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum WalError {
    #[error("WAL I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAL serialization error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("WAL deserialization error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("WAL payload size {0} exceeds max {MAX_WAL_PAYLOAD_LEN}")]
    PayloadTooLarge(usize),
    #[error("WAL corrupt segment {path} at offset {offset}: {reason}")]
    CorruptSegment {
        path: PathBuf,
        offset: usize,
        reason: String,
    },
    #[error("WAL non-monotonic LSN: expected > {last_lsn}, got {got_lsn}")]
    NonMonotonicLsn { last_lsn: u64, got_lsn: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum WalRecordKind {
    Mutation = 1,
    Checkpoint = 2,
    Undo = 3,
}

impl TryFrom<u16> for WalRecordKind {
    type Error = u16;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Mutation),
            2 => Ok(Self::Checkpoint),
            3 => Ok(Self::Undo),
            other => Err(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StateEffect {
    #[default]
    None,
    UpsertViewport(ViewportRecord),
    DeleteViewport(String),
    UpsertPatch(AstPatchRecord),
    UpsertCapsule(CausalCapsule),
    UpsertSymbolProvide(SymbolProvideEntry),
    RemoveSymbolProvide {
        symbol_fqn: String,
        viewport_id: String,
    },
    UpsertSymbolConsume(SymbolConsumeEntry),
    RemoveSymbolConsume {
        symbol_fqn: String,
        viewport_id: String,
    },
    InsertNegativeEdge(NegativeEdge),
    UpsertGitMap {
        key: String,
        entry: GitMapEntry,
    },
    RecordTrunkSnapshotTransition {
        pre_snapshot_cas: Blake3Hash,
        post_snapshot_cas: Blake3Hash,
    },
    Batch(Vec<StateEffect>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum InverseEffect {
    #[default]
    None,
    RestoreViewport(ViewportRecord),
    DeleteViewport(String),
    RestorePatch(AstPatchRecord),
    DeletePatch(String),
    RestoreCapsule(CausalCapsule),
    DeleteCapsule(String),
    RestoreSymbolProvide(SymbolProvideEntry),
    RemoveSymbolProvide {
        symbol_fqn: String,
        viewport_id: String,
    },
    RestoreSymbolConsume(SymbolConsumeEntry),
    RemoveSymbolConsume {
        symbol_fqn: String,
        viewport_id: String,
    },
    DeleteNegativeEdge {
        symbol_fqn: String,
        edge_seq: u64,
    },
    RestoreGitMap {
        key: String,
        entry: GitMapEntry,
    },
    DeleteGitMap {
        key: String,
    },
    RestoreTrunkSnapshot {
        pre_snapshot_cas: Blake3Hash,
        post_snapshot_cas: Blake3Hash,
    },
    RestoreHeadCapsule(Option<String>),
    Batch(Vec<InverseEffect>),
}

impl InverseEffect {
    pub fn collect_trunk_snapshot_hashes(&self, out: &mut std::collections::HashSet<Blake3Hash>) {
        match self {
            Self::RestoreTrunkSnapshot {
                pre_snapshot_cas,
                post_snapshot_cas,
            } => {
                out.insert(*pre_snapshot_cas);
                out.insert(*post_snapshot_cas);
            }
            Self::Batch(items) => {
                for item in items {
                    item.collect_trunk_snapshot_hashes(out);
                }
            }
            _ => {}
        }
    }

    pub fn collect_restore_viewport_cas_hashes(
        &self,
        is_live_vp: &impl Fn(&str) -> bool,
        cas_out: &mut std::collections::HashSet<Blake3Hash>,
        snapshot_manifests_out: &mut std::collections::HashSet<Blake3Hash>,
    ) {
        match self {
            Self::RestoreViewport(vp) => {
                if is_live_vp(&vp.id) {
                    if let Some(h) = vp.base_snapshot_cas {
                        cas_out.insert(h);
                        snapshot_manifests_out.insert(h);
                    }
                    if let Some(h) = vp.head_cas {
                        cas_out.insert(h);
                    }
                }
            }
            Self::Batch(items) => {
                for item in items {
                    item.collect_restore_viewport_cas_hashes(
                        is_live_vp,
                        cas_out,
                        snapshot_manifests_out,
                    );
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalPayload {
    InitRepo {
        meta: RepoMeta,
        #[serde(default)]
        init_op: Option<OpLogEntry>,
        #[serde(default)]
        git_seed: Option<(String, GitMapEntry)>,
    },
    Op {
        op: OpLogEntry,
        effect: StateEffect,
    },
    Undo {
        undo_op: OpLogEntry,
        target_op_seq: u64,
        inverse: InverseEffect,
    },
    Checkpoint {
        checkpoint_lsn: u64,
        last_applied_lsn: u64,
        last_op_seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    pub kind: WalRecordKind,
    pub lsn: u64,
    pub timestamp_ns: u64,
    pub payload: WalPayload,
}

#[derive(Debug, Clone)]
pub struct WalSegmentMeta {
    pub seg_id: u64,
    pub path: PathBuf,
    pub min_lsn: u64,
    pub max_lsn: u64,
    pub byte_len: u64,
}

#[derive(Debug, Clone)]
pub struct WalScanReport {
    pub records: Vec<WalRecord>,
    pub truncated_tail_bytes: u64,
    pub segments_scanned: usize,
    pub last_lsn: u64,
}

pub fn encode_wal_frame(
    kind: WalRecordKind,
    lsn: u64,
    timestamp_ns: u64,
    payload: &WalPayload,
) -> Result<Vec<u8>, WalError> {
    let payload_bytes = rmp_serde::to_vec_named(payload)?;
    if payload_bytes.len() > MAX_WAL_PAYLOAD_LEN {
        return Err(WalError::PayloadTooLarge(payload_bytes.len()));
    }
    let mut frame = Vec::with_capacity(WAL_FRAME_OVERHEAD + payload_bytes.len());
    frame.extend_from_slice(&WAL_MAGIC);
    frame.extend_from_slice(&WAL_VERSION.to_le_bytes());
    frame.extend_from_slice(&(kind as u16).to_le_bytes());
    frame.extend_from_slice(&lsn.to_le_bytes());
    frame.extend_from_slice(&timestamp_ns.to_le_bytes());
    frame.extend_from_slice(&(payload_bytes.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload_bytes);
    let mac = blake3::hash(&frame);
    frame.extend_from_slice(mac.as_bytes());
    Ok(frame)
}

pub fn segment_filename(seg_id: u64) -> String {
    format!("wal-{seg_id:016}.log")
}

pub fn parse_segment_filename(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("wal-")?.strip_suffix(".log")?;
    if rest.len() != 16 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u64>().ok()
}

#[derive(Debug)]
pub struct WalLog {
    wal_dir: PathBuf,
    segment_max_bytes: u64,
    active_seg_id: u64,
    active_file: File,
    current_seg_bytes: u64,
    last_lsn: u64,
    segments: Vec<WalSegmentMeta>,
}

impl WalLog {
    pub fn open_and_repair(
        wal_dir: impl Into<PathBuf>,
        segment_max_bytes: u64,
    ) -> Result<(Self, WalScanReport), WalError> {
        let wal_dir = wal_dir.into();
        std::fs::create_dir_all(&wal_dir)?;

        let mut discovered: Vec<(u64, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&wal_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if let Some(seg_id) = parse_segment_filename(&name) {
                discovered.push((seg_id, entry.path()));
            }
        }
        discovered.sort_by_key(|(id, _)| *id);

        if discovered.is_empty() {
            let first_id = 1u64;
            let first_path = wal_dir.join(segment_filename(first_id));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&first_path)?;
            let meta = WalSegmentMeta {
                seg_id: first_id,
                path: first_path,
                min_lsn: 0,
                max_lsn: 0,
                byte_len: 0,
            };
            let log = Self {
                wal_dir,
                segment_max_bytes: segment_max_bytes.max(1024),
                active_seg_id: first_id,
                active_file: file,
                current_seg_bytes: 0,
                last_lsn: 0,
                segments: vec![meta],
            };
            let report = WalScanReport {
                records: Vec::new(),
                truncated_tail_bytes: 0,
                segments_scanned: 1,
                last_lsn: 0,
            };
            return Ok((log, report));
        }

        let total_segments = discovered.len();
        let mut all_records = Vec::new();
        let mut segments_meta = Vec::with_capacity(total_segments);
        let mut truncated_tail_bytes = 0u64;
        let mut last_lsn = 0u64;

        for (idx, (seg_id, seg_path)) in discovered.iter().enumerate() {
            let is_final = idx + 1 == total_segments;
            let file_len = std::fs::metadata(seg_path)?.len();

            if file_len == 0 {
                segments_meta.push(WalSegmentMeta {
                    seg_id: *seg_id,
                    path: seg_path.clone(),
                    min_lsn: 0,
                    max_lsn: 0,
                    byte_len: 0,
                });
                continue;
            }

            let mut seg_min_lsn = 0u64;
            let mut seg_max_lsn = 0u64;
            let mut truncate_to: Option<usize> = None;

            // Inner scope guarantees Mmap + read File are closed before set_len / remove_file on Windows.
            {
                let read_file = File::open(seg_path)?;
                let mmap = unsafe { Mmap::map(&read_file)? };
                let slice: &[u8] = &mmap[..];
                let total_len = slice.len();
                let mut offset = 0usize;

                while offset < total_len {
                    let remaining = total_len - offset;
                    if remaining < WAL_HEADER_LEN {
                        if is_final && is_plausible_torn_header_prefix(&slice[offset..]) {
                            truncate_to = Some(offset);
                            break;
                        }
                        return Err(WalError::CorruptSegment {
                            path: seg_path.clone(),
                            offset,
                            reason: format!("truncated frame header ({remaining} bytes remaining)"),
                        });
                    }

                    let magic = &slice[offset..offset + 4];
                    let version = u16::from_le_bytes(
                        slice[offset + 4..offset + 6].try_into().unwrap(),
                    );
                    let kind_raw = u16::from_le_bytes(
                        slice[offset + 6..offset + 8].try_into().unwrap(),
                    );
                    let lsn = u64::from_le_bytes(
                        slice[offset + 8..offset + 16].try_into().unwrap(),
                    );
                    let timestamp_ns = u64::from_le_bytes(
                        slice[offset + 16..offset + 24].try_into().unwrap(),
                    );
                    let payload_len = u32::from_le_bytes(
                        slice[offset + 24..offset + 28].try_into().unwrap(),
                    ) as usize;

                    let header_valid = magic == WAL_MAGIC
                        && version == WAL_VERSION
                        && WalRecordKind::try_from(kind_raw).is_ok()
                        && payload_len <= MAX_WAL_PAYLOAD_LEN;

                    if !header_valid {
                        if is_final && slice[offset..].iter().all(|&b| b == 0) {
                            truncate_to = Some(offset);
                            break;
                        }
                        return Err(WalError::CorruptSegment {
                            path: seg_path.clone(),
                            offset,
                            reason: "invalid WAL frame header (magic/version/kind/len)".to_string(),
                        });
                    }

                    let expected_len = WAL_FRAME_OVERHEAD + payload_len;
                    if remaining < expected_len {
                        if is_final {
                            truncate_to = Some(offset);
                            break;
                        }
                        return Err(WalError::CorruptSegment {
                            path: seg_path.clone(),
                            offset,
                            reason: format!(
                                "truncated frame body (need {expected_len}, got {remaining})"
                            ),
                        });
                    }

                    let payload_end = offset + WAL_HEADER_LEN + payload_len;
                    let computed_mac = blake3::hash(&slice[offset..payload_end]);
                    let stored_mac = &slice[payload_end..offset + expected_len];
                    if computed_mac.as_bytes() != stored_mac {
                        if is_final && offset + expected_len == total_len {
                            truncate_to = Some(offset);
                            break;
                        }
                        return Err(WalError::CorruptSegment {
                            path: seg_path.clone(),
                            offset,
                            reason: "BLAKE3 MAC mismatch".to_string(),
                        });
                    }

                    let payload: WalPayload = match rmp_serde::from_slice(
                        &slice[offset + WAL_HEADER_LEN..payload_end],
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            if is_final && offset + expected_len == total_len {
                                truncate_to = Some(offset);
                                break;
                            }
                            return Err(WalError::CorruptSegment {
                                path: seg_path.clone(),
                                offset,
                                reason: format!("payload decode failure: {e}"),
                            });
                        }
                    };

                    let kind = WalRecordKind::try_from(kind_raw).unwrap();
                    if seg_min_lsn == 0 {
                        seg_min_lsn = lsn;
                    }
                    seg_max_lsn = lsn;
                    last_lsn = lsn;

                    all_records.push(WalRecord {
                        kind,
                        lsn,
                        timestamp_ns,
                        payload,
                    });

                    offset += expected_len;
                }
            }

            let final_byte_len = if let Some(trunc_off) = truncate_to {
                let trunc_u64 = trunc_off as u64;
                let file = OpenOptions::new().write(true).open(seg_path)?;
                file.set_len(trunc_u64)?;
                file.sync_all()?;
                truncated_tail_bytes += file_len.saturating_sub(trunc_u64);
                trunc_u64
            } else {
                file_len
            };

            segments_meta.push(WalSegmentMeta {
                seg_id: *seg_id,
                path: seg_path.clone(),
                min_lsn: seg_min_lsn,
                max_lsn: seg_max_lsn,
                byte_len: final_byte_len,
            });
        }

        let active_meta = segments_meta.last().expect("at least one segment exists");
        let active_seg_id = active_meta.seg_id;
        let current_seg_bytes = active_meta.byte_len;
        let active_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active_meta.path)?;

        let log = Self {
            wal_dir,
            segment_max_bytes: segment_max_bytes.max(1024),
            active_seg_id,
            active_file,
            current_seg_bytes,
            last_lsn,
            segments: segments_meta,
        };

        let report = WalScanReport {
            records: all_records,
            truncated_tail_bytes,
            segments_scanned: total_segments,
            last_lsn,
        };

        Ok((log, report))
    }

    pub fn last_lsn(&self) -> u64 {
        self.last_lsn
    }

    pub fn active_seg_id(&self) -> u64 {
        self.active_seg_id
    }

    pub fn segment_paths(&self) -> Vec<PathBuf> {
        self.segments.iter().map(|s| s.path.clone()).collect()
    }

    pub fn append(
        &mut self,
        kind: WalRecordKind,
        lsn: u64,
        timestamp_ns: u64,
        payload: &WalPayload,
    ) -> Result<(), WalError> {
        if lsn <= self.last_lsn {
            return Err(WalError::NonMonotonicLsn {
                last_lsn: self.last_lsn,
                got_lsn: lsn,
            });
        }
        let frame = encode_wal_frame(kind, lsn, timestamp_ns, payload)?;
        let frame_len = frame.len() as u64;

        if self.current_seg_bytes > 0
            && self.current_seg_bytes + frame_len > self.segment_max_bytes
        {
            self.rotate_segment()?;
        }

        self.active_file.write_all(&frame)?;
        if kind == WalRecordKind::Checkpoint {
            self.active_file.sync_all()?;
        } else {
            self.active_file.sync_data()?;
        }

        self.current_seg_bytes += frame_len;
        self.last_lsn = lsn;
        if let Some(active) = self.segments.last_mut() {
            if active.min_lsn == 0 {
                active.min_lsn = lsn;
            }
            active.max_lsn = lsn;
            active.byte_len = self.current_seg_bytes;
        }
        Ok(())
    }

    pub fn append_checkpoint(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        last_applied_lsn: u64,
        last_op_seq: u64,
    ) -> Result<(), WalError> {
        let payload = WalPayload::Checkpoint {
            checkpoint_lsn: lsn,
            last_applied_lsn,
            last_op_seq,
        };
        self.append(WalRecordKind::Checkpoint, lsn, timestamp_ns, &payload)
    }

    pub fn rotate_segment(&mut self) -> Result<(), WalError> {
        self.active_file.sync_all()?;
        let next_seg_id = self.active_seg_id + 1;
        let next_path = self.wal_dir.join(segment_filename(next_seg_id));
        let next_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&next_path)?;

        // Replace active_file so old handle is closed immediately.
        self.active_file = next_file;
        self.active_seg_id = next_seg_id;
        self.current_seg_bytes = 0;
        self.segments.push(WalSegmentMeta {
            seg_id: next_seg_id,
            path: next_path,
            min_lsn: 0,
            max_lsn: 0,
            byte_len: 0,
        });
        Ok(())
    }

    pub fn compact_up_to(&mut self, checkpoint_lsn: u64) -> Result<Vec<PathBuf>, WalError> {
        let active_id = self.active_seg_id;
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.segments.len());

        for seg in self.segments.drain(..) {
            let eligible = seg.seg_id < active_id && seg.max_lsn <= checkpoint_lsn;
            if eligible {
                match std::fs::remove_file(&seg.path) {
                    Ok(()) => removed.push(seg.path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(WalError::Io(e)),
                }
            } else {
                kept.push(seg);
            }
        }
        self.segments = kept;
        Ok(removed)
    }
}

fn is_plausible_torn_header_prefix(tail: &[u8]) -> bool {
    if tail.is_empty() {
        return true;
    }
    if tail.iter().all(|&b| b == 0) {
        return true;
    }
    if tail.len() < 4 {
        tail == &WAL_MAGIC[..tail.len()]
    } else {
        tail[..4] == WAL_MAGIC
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OpKind, now_ns};
    use tempfile::TempDir;

    fn sample_op_payload(lsn: u64) -> WalPayload {
        WalPayload::Op {
            op: OpLogEntry {
                op_seq: lsn,
                lsn,
                kind: OpKind::ViewportTransition,
                viewport_id: Some("vp-1".into()),
                target_id: None,
                capsule_id: None,
                touched_symbols: vec![],
                summary: format!("op-{lsn}"),
                inverse_payload: None,
                undone_by_op: None,
                timestamp_ns: now_ns(),
            },
            effect: StateEffect::None,
        }
    }

    #[test]
    fn torn_tail_truncated_cleanly_on_windows() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");

        let seg1_path;
        let valid_len;
        {
            let (mut wal, _) = WalLog::open_and_repair(&wal_dir, 1024 * 1024).unwrap();
            for lsn in 1..=3 {
                wal.append(WalRecordKind::Mutation, lsn, now_ns(), &sample_op_payload(lsn))
                    .unwrap();
            }
            seg1_path = wal.segment_paths()[0].clone();
            valid_len = std::fs::metadata(&seg1_path).unwrap().len();
        }

        // Append a torn partial frame starting with LMWA magic
        {
            let mut f = OpenOptions::new().append(true).open(&seg1_path).unwrap();
            f.write_all(b"LMWA\x01\x00\x01\x00partial_torn_bytes").unwrap();
            f.sync_all().unwrap();
        }
        assert!(std::fs::metadata(&seg1_path).unwrap().len() > valid_len);

        let (mut wal, report) = WalLog::open_and_repair(&wal_dir, 1024 * 1024).unwrap();
        assert_eq!(report.records.len(), 3);
        assert!(report.truncated_tail_bytes > 0);
        assert_eq!(std::fs::metadata(&seg1_path).unwrap().len(), valid_len);

        // Appending after repair succeeds without file lock errors
        wal.append(WalRecordKind::Mutation, 4, now_ns(), &sample_op_payload(4))
            .unwrap();
        assert_eq!(wal.last_lsn(), 4);
    }

    #[test]
    fn mid_stream_corruption_fails_loudly() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        let seg1_path;
        {
            let (mut wal, _) = WalLog::open_and_repair(&wal_dir, 1024 * 1024).unwrap();
            for lsn in 1..=3 {
                wal.append(WalRecordKind::Mutation, lsn, now_ns(), &sample_op_payload(lsn))
                    .unwrap();
            }
            seg1_path = wal.segment_paths()[0].clone();
        }

        // Flip 1 byte in record 1 (mid-stream payload)
        let mut bytes = std::fs::read(&seg1_path).unwrap();
        bytes[35] ^= 0xFF;
        std::fs::write(&seg1_path, &bytes).unwrap();

        let err = WalLog::open_and_repair(&wal_dir, 1024 * 1024).unwrap_err();
        assert!(matches!(err, WalError::CorruptSegment { .. }));

        // Also test corrupting magic bytes on a frame
        let dir2 = TempDir::new().unwrap();
        let wal_dir2 = dir2.path().join("wal");
        let seg_magic_path;
        {
            let (mut wal, _) = WalLog::open_and_repair(&wal_dir2, 1024 * 1024).unwrap();
            wal.append(WalRecordKind::Mutation, 1, now_ns(), &sample_op_payload(1))
                .unwrap();
            seg_magic_path = wal.segment_paths()[0].clone();
        }
        let mut b2 = std::fs::read(&seg_magic_path).unwrap();
        b2[0] = b'X';
        std::fs::write(&seg_magic_path, &b2).unwrap();
        let err2 = WalLog::open_and_repair(&wal_dir2, 1024 * 1024).unwrap_err();
        assert!(matches!(err2, WalError::CorruptSegment { .. }));
    }

    #[test]
    fn segment_rotation_and_compaction() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");

        let (mut wal, _) = WalLog::open_and_repair(&wal_dir, 1024).unwrap();
        let mut lsn = 1u64;
        for i in 1..=12u64 {
            wal.append(WalRecordKind::Mutation, lsn, now_ns(), &sample_op_payload(lsn))
                .unwrap();
            lsn += 1;
            if i == 6 {
                wal.append_checkpoint(lsn, now_ns(), lsn - 1, i).unwrap();
                lsn += 1;
            }
        }
        // Shift lsn continuity after checkpoint at 7
        assert!(wal.segment_paths().len() >= 3);
        let removed = wal.compact_up_to(7).unwrap();
        assert!(!removed.is_empty());
        for p in &removed {
            assert!(!p.exists());
        }
        // Active segment remains intact
        assert!(wal.segment_paths().last().unwrap().exists());
    }
}
