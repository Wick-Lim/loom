use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tempfile::NamedTempFile;
use thiserror::Error;

pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const DEFAULT_WAL_CHECKPOINT_OPS: u64 = 64;
pub const DEFAULT_WAL_SEGMENT_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_RPC_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config TOML parse error: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("config TOML serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("config temp file persist error: {0}")]
    Persist(#[from] tempfile::PersistError),
    #[error("invalid config value: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VfsDriverPreference {
    #[default]
    Auto,
    ApfsClonefile,
    LinuxReflink,
    LinuxFuseOverlayFs,
    WindowsRefsBlockClone,
    WindowsProjFs,
    WindowsNtfsFallback,
    GenericCopyFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VfsConfig {
    pub driver: VfsDriverPreference,
    pub cache_dirs: Vec<String>,
}

impl Default for VfsConfig {
    fn default() -> Self {
        Self {
            driver: VfsDriverPreference::Auto,
            cache_dirs: vec![
                "target".to_string(),
                "node_modules".to_string(),
                ".venv".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadarConfig {
    pub debounce_ms: u64,
    pub batch_window_ms: u64,
    pub batch_threshold_events: usize,
    pub ignore_dirs: Vec<String>,
}

impl Default for RadarConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 15,
            batch_window_ms: 200,
            batch_threshold_events: 20,
            ignore_dirs: vec![
                ".loom".to_string(),
                ".git".to_string(),
                "target".to_string(),
                "node_modules".to_string(),
                ".venv".to_string(),
                "dist".to_string(),
                "build".to_string(),
            ],
        }
    }
}

impl RadarConfig {
    pub fn normalized(&self) -> Self {
        let debounce_ms = self.debounce_ms.max(1);
        let batch_window_ms = self.batch_window_ms.max(debounce_ms);
        let batch_threshold_events = self.batch_threshold_events.max(2);
        Self {
            debounce_ms,
            batch_window_ms,
            batch_threshold_events,
            ignore_dirs: self.ignore_dirs.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub default_remote: String,
    pub notes_ref: String,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            default_remote: "origin".to_string(),
            notes_ref: "refs/notes/loom".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoomConfig {
    pub schema_version: u32,
    pub idle_timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
    pub zstd_level: i32,
    pub wal_checkpoint_ops: u64,
    pub wal_segment_max_bytes: u64,
    pub colocate_git: bool,
    pub max_rpc_frame_bytes: usize,
    pub vfs: VfsConfig,
    pub radar: RadarConfig,
    pub git: GitConfig,
}

impl Default for LoomConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            idle_timeout_ms: None,
            zstd_level: DEFAULT_ZSTD_LEVEL,
            wal_checkpoint_ops: DEFAULT_WAL_CHECKPOINT_OPS,
            wal_segment_max_bytes: DEFAULT_WAL_SEGMENT_MAX_BYTES,
            colocate_git: false,
            max_rpc_frame_bytes: DEFAULT_MAX_RPC_FRAME_BYTES,
            vfs: VfsConfig::default(),
            radar: RadarConfig::default(),
            git: GitConfig::default(),
        }
    }
}

impl LoomConfig {
    pub fn idle_timeout(&self) -> Duration {
        match self.idle_timeout_ms {
            Some(ms) => Duration::from_millis(ms),
            None => Duration::from_secs(self.idle_timeout_secs),
        }
    }

    pub fn parse(toml_str: &str) -> Result<Self, ConfigError> {
        let mut cfg: Self = toml::from_str(toml_str)?;
        if cfg.radar.debounce_ms >= 1 && cfg.radar.batch_window_ms < cfg.radar.debounce_ms {
            cfg.radar.batch_window_ms = cfg.radar.debounce_ms;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        self.validate()?;
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let content = toml::to_string_pretty(self)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut tmp = NamedTempFile::new_in(parent)?;
        tmp.write_all(content.as_bytes())?;
        tmp.as_file().sync_data()?;
        tmp.persist(path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=22).contains(&self.zstd_level) {
            return Err(ConfigError::Validation(format!(
                "zstd_level {} must be in 1..=22",
                self.zstd_level
            )));
        }
        if self.wal_checkpoint_ops < 1 {
            return Err(ConfigError::Validation(
                "wal_checkpoint_ops must be >= 1".to_string(),
            ));
        }
        if self.wal_segment_max_bytes < 1024 {
            return Err(ConfigError::Validation(
                "wal_segment_max_bytes must be >= 1024".to_string(),
            ));
        }
        if self.max_rpc_frame_bytes < 1024 {
            return Err(ConfigError::Validation(
                "max_rpc_frame_bytes must be >= 1024".to_string(),
            ));
        }
        if self.radar.debounce_ms < 1 {
            return Err(ConfigError::Validation(
                "radar.debounce_ms must be >= 1".to_string(),
            ));
        }
        if self.radar.batch_window_ms < self.radar.debounce_ms {
            return Err(ConfigError::Validation(format!(
                "radar.batch_window_ms ({}) must be >= radar.debounce_ms ({})",
                self.radar.batch_window_ms, self.radar.debounce_ms
            )));
        }
        if self.radar.batch_threshold_events < 2 {
            return Err(ConfigError::Validation(
                "radar.batch_threshold_events must be >= 2".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_config_values_and_idle_timeout() {
        let cfg = LoomConfig::default();
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.idle_timeout_secs, 1800);
        assert_eq!(cfg.idle_timeout(), Duration::from_secs(1800));
        assert_eq!(cfg.zstd_level, 3);
        assert_eq!(cfg.wal_checkpoint_ops, 64);
        assert_eq!(cfg.wal_segment_max_bytes, 16 * 1024 * 1024);
        assert!(!cfg.colocate_git);
        assert_eq!(cfg.vfs.driver, VfsDriverPreference::Auto);
        assert!(cfg.validate().is_ok());

        let mut ms_cfg = cfg.clone();
        ms_cfg.idle_timeout_ms = Some(250);
        assert_eq!(ms_cfg.idle_timeout(), Duration::from_millis(250));
    }

    #[test]
    fn config_parse_and_atomic_save_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".loom/config.toml");
        let cfg = LoomConfig {
            colocate_git: true,
            idle_timeout_ms: Some(500),
            wal_segment_max_bytes: 4096,
            ..LoomConfig::default()
        };

        cfg.save(&path).unwrap();
        let loaded = LoomConfig::load(&path).unwrap();
        assert_eq!(cfg, loaded);

        let partial = LoomConfig::parse("colocate_git = true\n").unwrap();
        assert!(partial.colocate_git);
        assert_eq!(partial.idle_timeout_secs, 1800);
    }

    #[test]
    fn config_validation_rejects_bad_values() {
        let mut cfg = LoomConfig {
            zstd_level: 0,
            ..LoomConfig::default()
        };
        assert!(cfg.validate().is_err());

        cfg.zstd_level = 3;
        cfg.wal_checkpoint_ops = 0;
        assert!(cfg.validate().is_err());

        cfg.wal_checkpoint_ops = 64;
        cfg.wal_segment_max_bytes = 128;
        assert!(cfg.validate().is_err());

        cfg.wal_segment_max_bytes = DEFAULT_WAL_SEGMENT_MAX_BYTES;
        cfg.radar.debounce_ms = 0;
        assert!(cfg.validate().is_err());

        cfg.radar.debounce_ms = 50;
        cfg.radar.batch_window_ms = 20;
        assert!(cfg.validate().is_err());

        cfg.radar.batch_window_ms = 200;
        cfg.radar.batch_threshold_events = 1;
        assert!(cfg.validate().is_err());

        let clamped = RadarConfig {
            debounce_ms: 0,
            batch_window_ms: 0,
            batch_threshold_events: 0,
            ignore_dirs: vec![".git".to_string()],
        }
        .normalized();
        assert_eq!(clamped.debounce_ms, 1);
        assert_eq!(clamped.batch_window_ms, 1);
        assert_eq!(clamped.batch_threshold_events, 2);
    }

    #[test]
    fn parse_promotes_batch_window_ms_when_debounce_exceeds_default() {
        let toml_str = r#"
[radar]
debounce_ms = 500
batch_window_ms = 200
"#;
        let parsed = LoomConfig::parse(toml_str).unwrap();
        assert_eq!(parsed.radar.debounce_ms, 500);
        assert_eq!(parsed.radar.batch_window_ms, 500);

        let zero_debounce = r#"
[radar]
debounce_ms = 0
batch_window_ms = 200
"#;
        assert!(LoomConfig::parse(zero_debounce).is_err());
    }
}
