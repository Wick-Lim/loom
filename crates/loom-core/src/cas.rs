use crate::model::{BinaryManifest, Blake3Hash, LanguageKind};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
const MAX_PREALLOC_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum CasError {
    #[error("CAS object not found: {0}")]
    NotFound(Blake3Hash),
    #[error("CAS hash mismatch: expected {expected}, actual {actual}")]
    HashMismatch {
        expected: Blake3Hash,
        actual: Blake3Hash,
    },
    #[error("CAS length mismatch: expected {expected} bytes, actual {actual} bytes")]
    LengthMismatch { expected: u64, actual: u64 },
    #[error("invalid binary manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid zstd compression level: {0} (must be 1..=22)")]
    InvalidZstdLevel(i32),
    #[error("invalid chunk size: {0}")]
    InvalidChunkSize(usize),
    #[error("CAS I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CAS temp file persist error: {0}")]
    Persist(#[from] tempfile::PersistError),
}

pub fn is_binary_content(path: &Path, sample: &[u8]) -> bool {
    LanguageKind::detect(path, sample) == LanguageKind::Binary
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CasStats {
    pub object_count: u64,
    pub compressed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct CasStore {
    root: PathBuf,
    zstd_level: i32,
}

impl CasStore {
    pub fn new(root: impl Into<PathBuf>, zstd_level: i32) -> Result<Self, CasError> {
        if !(1..=22).contains(&zstd_level) {
            return Err(CasError::InvalidZstdLevel(zstd_level));
        }
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, zstd_level })
    }

    pub fn open_default(root: impl Into<PathBuf>) -> Result<Self, CasError> {
        Self::new(root, DEFAULT_ZSTD_LEVEL)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_path(&self, hash: &Blake3Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(format!("{}.zst", &hex[2..]))
    }

    pub fn put_bytes(&self, data: &[u8]) -> Result<Blake3Hash, CasError> {
        let hash = Blake3Hash::from_bytes(data);
        let obj_path = self.object_path(&hash);
        if obj_path.is_file() {
            return Ok(hash);
        }

        let shard_dir = obj_path
            .parent()
            .expect("CAS object path always has shard parent");
        std::fs::create_dir_all(shard_dir)?;

        let compressed = zstd::stream::encode_all(std::io::Cursor::new(data), self.zstd_level)?;
        let mut tmp = NamedTempFile::new_in(shard_dir)?;
        tmp.write_all(&compressed)?;
        tmp.as_file().sync_data()?;

        match tmp.persist_noclobber(&obj_path) {
            Ok(_) => Ok(hash),
            Err(err) if obj_path.is_file() => {
                drop(err);
                Ok(hash)
            }
            Err(err) => Err(CasError::Persist(err)),
        }
    }

    pub fn get_bytes(&self, hash: &Blake3Hash) -> Result<Vec<u8>, CasError> {
        let obj_path = self.object_path(hash);
        let compressed = match std::fs::read(&obj_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CasError::NotFound(*hash));
            }
            Err(e) => return Err(CasError::Io(e)),
        };

        let plaintext = zstd::stream::decode_all(std::io::Cursor::new(&compressed))?;
        let actual = Blake3Hash::from_bytes(&plaintext);
        if actual != *hash {
            return Err(CasError::HashMismatch {
                expected: *hash,
                actual,
            });
        }
        Ok(plaintext)
    }

    pub fn get_raw_compressed(&self, hash: &Blake3Hash) -> Result<Vec<u8>, CasError> {
        let obj_path = self.object_path(hash);
        match std::fs::read(&obj_path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CasError::NotFound(*hash)),
            Err(e) => Err(CasError::Io(e)),
        }
    }

    pub fn put_verified_compressed(
        &self,
        expected_hash: &Blake3Hash,
        zstd_bytes: &[u8],
    ) -> Result<bool, CasError> {
        let obj_path = self.object_path(expected_hash);
        if obj_path.is_file() {
            return Ok(false);
        }

        let plaintext = zstd::stream::decode_all(std::io::Cursor::new(zstd_bytes))?;
        let actual = Blake3Hash::from_bytes(&plaintext);
        if actual != *expected_hash {
            return Err(CasError::HashMismatch {
                expected: *expected_hash,
                actual,
            });
        }

        let shard_dir = obj_path
            .parent()
            .expect("CAS object path always has shard parent");
        std::fs::create_dir_all(shard_dir)?;

        let mut tmp = NamedTempFile::new_in(shard_dir)?;
        tmp.write_all(zstd_bytes)?;
        tmp.as_file().sync_data()?;

        match tmp.persist_noclobber(&obj_path) {
            Ok(_) => Ok(true),
            Err(err) if obj_path.is_file() => {
                drop(err);
                Ok(false)
            }
            Err(err) => Err(CasError::Persist(err)),
        }
    }

    pub fn contains(&self, hash: &Blake3Hash) -> bool {
        self.object_path(hash).is_file()
    }

    pub fn delete(&self, hash: &Blake3Hash) -> Result<bool, CasError> {
        let obj_path = self.object_path(hash);
        match std::fs::remove_file(&obj_path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(CasError::Io(e)),
        }
    }

    pub fn list_hashes(&self) -> Result<Vec<Blake3Hash>, CasError> {
        let mut hashes = Vec::new();
        if !self.root.exists() {
            return Ok(hashes);
        }

        for shard_entry in std::fs::read_dir(&self.root)? {
            let shard_entry = shard_entry?;
            if !shard_entry.file_type()?.is_dir() {
                continue;
            }
            let shard_name = shard_entry.file_name();
            let Some(prefix) = shard_name.to_str() else {
                continue;
            };
            if prefix.len() != 2 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }

            for obj_entry in std::fs::read_dir(shard_entry.path())? {
                let obj_entry = obj_entry?;
                if !obj_entry.file_type()?.is_file() {
                    continue;
                }
                let file_name = obj_entry.file_name();
                let Some(name_str) = file_name.to_str() else {
                    continue;
                };
                if name_str.contains(".tmp") {
                    continue;
                }
                let Some(rest) = name_str.strip_suffix(".zst") else {
                    continue;
                };
                if rest.len() != 62 {
                    continue;
                }
                let full_hex = format!("{prefix}{rest}");
                if let Ok(hash) = Blake3Hash::from_hex(&full_hex) {
                    hashes.push(hash);
                }
            }
        }

        hashes.sort();
        Ok(hashes)
    }

    pub fn stats(&self) -> Result<CasStats, CasError> {
        let mut stats = CasStats::default();
        for hash in self.list_hashes()? {
            let path = self.object_path(&hash);
            if let Ok(meta) = std::fs::metadata(path) {
                stats.object_count += 1;
                stats.compressed_bytes += meta.len();
            }
        }
        Ok(stats)
    }

    pub fn put_chunked_binary(
        &self,
        data: &[u8],
        chunk_size: usize,
    ) -> Result<BinaryManifest, CasError> {
        if chunk_size == 0 {
            return Err(CasError::InvalidChunkSize(chunk_size));
        }
        let chunk_size_u32 = u32::try_from(chunk_size)
            .map_err(|_| CasError::InvalidChunkSize(chunk_size))?;
        let file_hash = Blake3Hash::from_bytes(data);
        let mut chunk_hashes = Vec::new();
        if data.is_empty() {
            chunk_hashes.push(self.put_bytes(&[])?);
        } else {
            for chunk in data.chunks(chunk_size) {
                chunk_hashes.push(self.put_bytes(chunk)?);
            }
        }
        Ok(BinaryManifest {
            total_len: data.len() as u64,
            chunk_size: chunk_size_u32,
            file_hash,
            chunk_hashes,
        })
    }

    pub fn put_chunked_binary_default(&self, data: &[u8]) -> Result<BinaryManifest, CasError> {
        self.put_chunked_binary(data, DEFAULT_CHUNK_SIZE)
    }

    pub fn get_chunked_binary(&self, manifest: &BinaryManifest) -> Result<Vec<u8>, CasError> {
        if manifest.chunk_hashes.is_empty() {
            return Err(CasError::InvalidManifest(
                "chunk_hashes must not be empty".into(),
            ));
        }
        let prealloc = usize::try_from(manifest.total_len)
            .unwrap_or(MAX_PREALLOC_BYTES)
            .min(MAX_PREALLOC_BYTES);
        let mut out = Vec::with_capacity(prealloc);
        for chunk_hash in &manifest.chunk_hashes {
            let chunk = self.get_bytes(chunk_hash)?;
            out.extend_from_slice(&chunk);
        }
        let actual_len = out.len() as u64;
        if actual_len != manifest.total_len {
            return Err(CasError::LengthMismatch {
                expected: manifest.total_len,
                actual: actual_len,
            });
        }
        let actual = Blake3Hash::from_bytes(&out);
        if actual != manifest.file_hash {
            return Err(CasError::HashMismatch {
                expected: manifest.file_hash,
                actual,
            });
        }
        Ok(out)
    }

    pub fn put_binary_file(
        &self,
        path: &Path,
        chunk_size: usize,
    ) -> Result<BinaryManifest, CasError> {
        if chunk_size == 0 {
            return Err(CasError::InvalidChunkSize(chunk_size));
        }
        let chunk_size_u32 = u32::try_from(chunk_size)
            .map_err(|_| CasError::InvalidChunkSize(chunk_size))?;
        let mut file = File::open(path)?;
        let mut buf = vec![0u8; chunk_size];
        let mut hasher = blake3::Hasher::new();
        let mut chunk_hashes = Vec::new();
        let mut total_len: u64 = 0;

        loop {
            let n = read_chunk(&mut file, &mut buf)?;
            if n == 0 {
                break;
            }
            let slice = &buf[..n];
            hasher.update(slice);
            total_len += n as u64;
            chunk_hashes.push(self.put_bytes(slice)?);
        }

        if chunk_hashes.is_empty() {
            chunk_hashes.push(self.put_bytes(&[])?);
        }

        Ok(BinaryManifest {
            total_len,
            chunk_size: chunk_size_u32,
            file_hash: Blake3Hash(*hasher.finalize().as_bytes()),
            chunk_hashes,
        })
    }

    pub fn get_binary_file(
        &self,
        manifest: &BinaryManifest,
        dest: &Path,
    ) -> Result<(), CasError> {
        if manifest.chunk_hashes.is_empty() {
            return Err(CasError::InvalidManifest(
                "chunk_hashes must not be empty".into(),
            ));
        }
        let parent = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => {
                std::fs::create_dir_all(p)?;
                p
            }
            _ => Path::new("."),
        };

        let mut tmp = NamedTempFile::new_in(parent)?;
        let mut hasher = blake3::Hasher::new();
        let mut actual_len: u64 = 0;

        for chunk_hash in &manifest.chunk_hashes {
            let chunk = self.get_bytes(chunk_hash)?;
            hasher.update(&chunk);
            actual_len += chunk.len() as u64;
            tmp.write_all(&chunk)?;
        }

        if actual_len != manifest.total_len {
            return Err(CasError::LengthMismatch {
                expected: manifest.total_len,
                actual: actual_len,
            });
        }

        let actual_hash = Blake3Hash(*hasher.finalize().as_bytes());
        if actual_hash != manifest.file_hash {
            return Err(CasError::HashMismatch {
                expected: manifest.file_hash,
                actual: actual_hash,
            });
        }

        tmp.as_file().sync_data()?;
        tmp.persist(dest)?;
        Ok(())
    }

    pub fn verify_binary_manifest(&self, manifest: &BinaryManifest) -> Result<bool, CasError> {
        if manifest.chunk_hashes.is_empty() {
            return Ok(false);
        }
        let mut hasher = blake3::Hasher::new();
        let mut actual_len: u64 = 0;
        for chunk_hash in &manifest.chunk_hashes {
            match self.get_bytes(chunk_hash) {
                Ok(chunk) => {
                    hasher.update(&chunk);
                    actual_len += chunk.len() as u64;
                }
                Err(CasError::NotFound(_)) | Err(CasError::HashMismatch { .. }) => {
                    return Ok(false);
                }
                Err(e) => return Err(e),
            }
        }
        if actual_len != manifest.total_len {
            return Ok(false);
        }
        let actual_hash = Blake3Hash(*hasher.finalize().as_bytes());
        Ok(actual_hash == manifest.file_hash)
    }
}

fn read_chunk(reader: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn cas_put_get_zero_and_large_dedup_and_stats() {
        let dir = TempDir::new().unwrap();
        let cas = CasStore::open_default(dir.path().join("cas")).unwrap();

        let h_empty = cas.put_bytes(&[]).unwrap();
        assert!(cas.contains(&h_empty));
        assert_eq!(cas.get_bytes(&h_empty).unwrap(), Vec::<u8>::new());

        let payload = vec![b'L'; 128 * 1024];
        let h1 = cas.put_bytes(&payload).unwrap();
        let h2 = cas.put_bytes(&payload).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(cas.get_bytes(&h1).unwrap(), payload);

        let hex = h1.to_hex();
        let expected_path = dir
            .path()
            .join("cas")
            .join(&hex[..2])
            .join(format!("{}.zst", &hex[2..]));
        assert_eq!(cas.object_path(&h1), expected_path);
        assert!(expected_path.is_file());

        let listed = cas.list_hashes().unwrap();
        assert_eq!(listed.len(), 2);

        let stats = cas.stats().unwrap();
        assert_eq!(stats.object_count, 2);
        assert!(stats.compressed_bytes > 0);
        assert!(stats.compressed_bytes < payload.len() as u64);

        assert!(cas.delete(&h_empty).unwrap());
        assert!(!cas.delete(&h_empty).unwrap());
        assert!(!cas.contains(&h_empty));
    }

    #[test]
    fn cas_detects_corruption_with_hash_mismatch() {
        let dir = TempDir::new().unwrap();
        let cas = CasStore::open_default(dir.path().join("cas")).unwrap();
        let hash = cas.put_bytes(b"authentic content").unwrap();

        let obj_path = cas.object_path(&hash);
        let tampered = zstd::stream::encode_all(std::io::Cursor::new(b"tampered content"), 3)
            .unwrap();
        std::fs::write(&obj_path, tampered).unwrap();

        let err = cas.get_bytes(&hash).unwrap_err();
        assert!(matches!(err, CasError::HashMismatch { .. }));
    }

    #[test]
    fn cas_chunked_binary_roundtrip() {
        let dir = TempDir::new().unwrap();
        let cas = CasStore::open_default(dir.path().join("cas")).unwrap();
        let data: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();
        let manifest = cas.put_chunked_binary(&data, 256).unwrap();
        assert_eq!(manifest.chunk_hashes.len(), 4);
        let restored = cas.get_chunked_binary(&manifest).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn cas_binary_streaming_hardening_and_detection() {
        let dir = TempDir::new().unwrap();
        let cas = CasStore::open_default(dir.path().join("cas")).unwrap();

        // Multi-chunk default (64 KiB chunks -> 150 KiB = 3 chunks)
        let large: Vec<u8> = (0..(150 * 1024)).map(|i| ((i * 7) % 256) as u8).collect();
        let manifest = cas.put_chunked_binary_default(&large).unwrap();
        assert_eq!(manifest.chunk_size as usize, DEFAULT_CHUNK_SIZE);
        assert_eq!(manifest.chunk_hashes.len(), 3);
        assert!(cas.verify_binary_manifest(&manifest).unwrap());
        assert_eq!(cas.get_chunked_binary(&manifest).unwrap(), large);

        // LengthMismatch guard
        let mut bad_len = manifest.clone();
        bad_len.total_len += 1;
        assert!(matches!(
            cas.get_chunked_binary(&bad_len),
            Err(CasError::LengthMismatch { .. })
        ));
        assert!(!cas.verify_binary_manifest(&bad_len).unwrap());

        // Empty chunk_hashes guard
        let empty_chunks = BinaryManifest {
            total_len: 0,
            chunk_size: 64,
            file_hash: Blake3Hash::ZERO,
            chunk_hashes: Vec::new(),
        };
        assert!(matches!(
            cas.get_chunked_binary(&empty_chunks),
            Err(CasError::InvalidManifest(_))
        ));

        // Streaming file put / get
        let src_path = dir.path().join("asset.bin");
        let dst_path = dir.path().join("nested").join("restored.bin");
        std::fs::write(&src_path, &large).unwrap();
        let file_manifest = cas.put_binary_file(&src_path, 32 * 1024).unwrap();
        assert_eq!(file_manifest.file_hash, manifest.file_hash);
        assert_eq!(file_manifest.chunk_hashes.len(), 5);
        cas.get_binary_file(&file_manifest, &dst_path).unwrap();
        assert_eq!(std::fs::read(&dst_path).unwrap(), large);

        // 0-byte file streaming
        let zero_src = dir.path().join("empty.bin");
        let zero_dst = dir.path().join("empty_out.bin");
        std::fs::write(&zero_src, b"").unwrap();
        let zero_manifest = cas.put_binary_file(&zero_src, 1024).unwrap();
        assert_eq!(zero_manifest.total_len, 0);
        assert_eq!(zero_manifest.chunk_hashes.len(), 1);
        cas.get_binary_file(&zero_manifest, &zero_dst).unwrap();
        assert_eq!(std::fs::read(&zero_dst).unwrap(), b"");

        // Binary detection
        assert!(is_binary_content(Path::new("img.png"), b"not nul"));
        assert!(is_binary_content(Path::new("data.txt"), b"hello\0world"));
        assert!(is_binary_content(Path::new("raw.txt"), &[0xff, 0xfe, 0xfd]));
        assert!(!is_binary_content(Path::new("main.rs"), b"fn main() {}\n"));
        assert_eq!(
            LanguageKind::detect(Path::new("app.tsx"), b"export const C = () => <div />;"),
            LanguageKind::Tsx
        );

        // NUL byte after 8192-byte UTF-8 prefix must still detect as Binary
        let mut late_nul = vec![b'a'; 8500];
        late_nul.push(0);
        late_nul.extend_from_slice(b"tail");
        assert!(is_binary_content(Path::new("notes.txt"), &late_nul));

        // chunk_size > u32::MAX must fail instead of truncating
        #[cfg(target_pointer_width = "64")]
        {
            assert!(matches!(
                cas.put_chunked_binary(b"x", (u32::MAX as usize) + 1),
                Err(CasError::InvalidChunkSize(_))
            ));
        }
    }

    #[test]
    fn cas_raw_compressed_and_verified_put_roundtrip() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src = CasStore::open_default(src_dir.path().join("cas")).unwrap();
        let dst = CasStore::open_default(dst_dir.path().join("cas")).unwrap();

        let payload = b"loom native remote zstd payload";
        let hash = src.put_bytes(payload).unwrap();
        let raw_zst = src.get_raw_compressed(&hash).unwrap();

        assert!(dst.put_verified_compressed(&hash, &raw_zst).unwrap());
        assert!(!dst.put_verified_compressed(&hash, &raw_zst).unwrap());
        assert_eq!(dst.get_bytes(&hash).unwrap(), payload);

        let wrong_hash = Blake3Hash::from_bytes(b"different content");
        let err = dst.put_verified_compressed(&wrong_hash, &raw_zst).unwrap_err();
        assert!(matches!(err, CasError::HashMismatch { .. }));
        assert!(!dst.contains(&wrong_hash));
    }
}
