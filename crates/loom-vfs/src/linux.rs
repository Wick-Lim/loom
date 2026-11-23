use crate::{
    DEFAULT_CACHE_DIRS, ViewportDriver, clean_viewport_for_restore, is_isolated_lockfile,
    remove_dir_all_safe,
};
use loom_core::model::{CowBackendKind, ViewportMount};
use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

const BTRFS_SUPER_MAGIC: u64 = 0x9123_683E;
const XFS_SUPER_MAGIC: u64 = 0x5846_5342;
const BCACHEFS_SUPER_MAGIC: u64 = 0xCA45_1A4E;

pub struct LinuxViewportDriver {
    backend: CowBackendKind,
    default_cache_dirs: Vec<String>,
}

impl LinuxViewportDriver {
    pub fn detect() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::detect_for_path(&cwd)
    }

    pub fn detect_for_path(repo_root: &Path) -> Self {
        let backend = Self::probe_backend_for_path(repo_root);
        Self::with_backend(backend)
    }

    pub fn with_backend(backend: CowBackendKind) -> Self {
        Self {
            backend,
            default_cache_dirs: DEFAULT_CACHE_DIRS.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    pub fn with_cache_dirs(mut self, cache_dirs: Vec<String>) -> Self {
        if !cache_dirs.is_empty() {
            self.default_cache_dirs = cache_dirs;
        }
        self
    }

    fn probe_backend_for_path(repo_root: &Path) -> CowBackendKind {
        let probe_target = repo_root
            .ancestors()
            .find(|p| !p.as_os_str().is_empty() && p.exists())
            .unwrap_or_else(|| Path::new("."));
        if let Ok(st) = rustix::fs::statfs(probe_target) {
            let ftype = st.f_type as u64;
            if matches!(
                ftype,
                BTRFS_SUPER_MAGIC | XFS_SUPER_MAGIC | BCACHEFS_SUPER_MAGIC
            ) {
                return CowBackendKind::LinuxReflink;
            }
        }
        if Path::new("/dev/fuse").exists() && find_binary_in_path("fuse-overlayfs") {
            return CowBackendKind::LinuxFuseOverlayFs;
        }
        CowBackendKind::GenericCopyFallback
    }

    fn overlay_dirs_for(viewport_root: &Path) -> (PathBuf, PathBuf) {
        let parent = viewport_root.parent().unwrap_or_else(|| Path::new("."));
        let name = viewport_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("viewport");
        (
            parent.join(format!(".{name}.upper")),
            parent.join(format!(".{name}.work")),
        )
    }

    fn materialize_reflink_or_copy(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        std::fs::create_dir_all(viewport_root)?;
        let cache_set: HashSet<&str> = cache_dirs.iter().copied().collect();
        let mut copied_files = 0u64;
        self.walk_reflink(
            trunk_root,
            trunk_root,
            viewport_root,
            &cache_set,
            false,
            false,
            &mut copied_files,
        )?;
        Ok(ViewportMount {
            backend: self.backend,
            viewport_root: viewport_root.to_path_buf(),
            mount_point: None,
            upper_dir: None,
            work_dir: None,
            hardlinked_files: 0,
            copied_files,
            shared_cache_dirs: cache_dirs.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_reflink(
        &self,
        trunk_root: &Path,
        src: &Path,
        dst: &Path,
        cache_set: &HashSet<&str>,
        in_cache_dir: bool,
        in_target_dir: bool,
        copied_files: &mut u64,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(dst)?;
        if !src.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name_str = file_name.to_str();
            if src == trunk_root && matches!(name_str, Some(".loom" | ".git")) {
                continue;
            }
            let src_path = entry.path();
            let dst_path = dst.join(&file_name);
            let ft = entry.file_type()?;
            let is_root_cache =
                src == trunk_root && name_str.is_some_and(|n| cache_set.contains(n));
            let child_in_cache = in_cache_dir || is_root_cache;
            let child_in_target =
                in_target_dir || (src == trunk_root && name_str == Some("target"));

            if ft.is_dir() {
                if child_in_target && name_str == Some("incremental") {
                    continue;
                }
                self.walk_reflink(
                    trunk_root,
                    &src_path,
                    &dst_path,
                    cache_set,
                    child_in_cache,
                    child_in_target,
                    copied_files,
                )?;
            } else if ft.is_file() {
                if child_in_cache && is_isolated_lockfile(Path::new(&file_name)) {
                    continue;
                }
                if self.backend == CowBackendKind::LinuxReflink {
                    self.copy_on_write_file(&src_path, &dst_path)?;
                } else {
                    std::fs::copy(&src_path, &dst_path)?;
                }
                *copied_files += 1;
            }
        }
        Ok(())
    }

    fn materialize_fuse_overlay(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        let (upper_dir, work_dir) = Self::overlay_dirs_for(viewport_root);
        std::fs::create_dir_all(viewport_root)?;
        std::fs::create_dir_all(&upper_dir)?;
        std::fs::create_dir_all(&work_dir)?;

        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            trunk_root.display(),
            upper_dir.display(),
            work_dir.display()
        );
        let status = Command::new("fuse-overlayfs")
            .arg("-o")
            .arg(&opts)
            .arg(viewport_root)
            .status()?;
        anyhow::ensure!(
            status.success(),
            "fuse-overlayfs failed to mount {} (status: {status})",
            viewport_root.display()
        );

        // Mask .loom, .git, and isolated cache lockfiles / incremental dirs inside the overlay upper layer
        for hidden in [".loom", ".git"] {
            let masked = viewport_root.join(hidden);
            if masked.exists() {
                let _ = remove_dir_all_safe(&masked);
            }
        }
        for cache in cache_dirs {
            let cache_root = viewport_root.join(cache);
            if cache_root.is_dir() {
                Self::mask_fuse_cache_dir(&cache_root, *cache == "target")?;
            }
        }

        Ok(ViewportMount {
            backend: CowBackendKind::LinuxFuseOverlayFs,
            viewport_root: viewport_root.to_path_buf(),
            mount_point: Some(viewport_root.to_path_buf()),
            upper_dir: Some(upper_dir),
            work_dir: Some(work_dir),
            hardlinked_files: 0,
            copied_files: 0,
            shared_cache_dirs: cache_dirs.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    fn mask_fuse_cache_dir(dir: &Path, in_target_dir: bool) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name_str = file_name.to_str();
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if in_target_dir && name_str == Some("incremental") {
                    let _ = remove_dir_all_safe(&path);
                    continue;
                }
                Self::mask_fuse_cache_dir(&path, in_target_dir)?;
            } else if ft.is_file() && is_isolated_lockfile(Path::new(&file_name)) {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(())
    }

    fn snapshot_loom_dir(loom_dir: &Path) -> anyhow::Result<Option<Vec<(PathBuf, Vec<u8>)>>> {
        if !loom_dir.is_dir() {
            return Ok(None);
        }
        let mut out = Vec::new();
        Self::collect_dir_files(loom_dir, loom_dir, &mut out)?;
        Ok(Some(out))
    }

    fn collect_dir_files(
        root: &Path,
        dir: &Path,
        out: &mut Vec<(PathBuf, Vec<u8>)>,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                Self::collect_dir_files(root, &path, out)?;
            } else if ft.is_file() {
                let rel = path.strip_prefix(root)?.to_path_buf();
                let bytes = std::fs::read(&path)?;
                out.push((rel, bytes));
            }
        }
        Ok(())
    }

    fn restore_loom_dir(
        loom_dir: &Path,
        snapshot: Option<&[(PathBuf, Vec<u8>)]>,
    ) -> anyhow::Result<()> {
        let Some(files) = snapshot else {
            return Ok(());
        };
        std::fs::create_dir_all(loom_dir)?;
        for (rel, bytes) in files {
            let dst = loom_dir.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dst, bytes)?;
        }
        Ok(())
    }

    fn is_fuse_mounted(mount_point: &Path) -> Option<bool> {
        let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
        let canon = std::fs::canonicalize(mount_point).unwrap_or_else(|_| mount_point.to_path_buf());
        let mp_str = canon.to_string_lossy();
        let raw_str = mount_point.to_string_lossy();
        Some(mounts.lines().any(|line| {
            let mut parts = line.split_whitespace();
            let _dev = parts.next();
            let Some(target) = parts.next() else {
                return false;
            };
            let unescaped = target.replace("\\040", " ");
            unescaped == mp_str.as_ref() || unescaped == raw_str.as_ref()
        }))
    }

    fn unmount_fuse(mount_point: &Path, ignore_not_mounted: bool) -> anyhow::Result<()> {
        if ignore_not_mounted && Self::is_fuse_mounted(mount_point) == Some(false) {
            return Ok(());
        }
        let mut last_err = None;
        for bin in ["fusermount3", "fusermount"] {
            match Command::new(bin).arg("-u").arg(mount_point).output() {
                Ok(out) if out.status.success() => return Ok(()),
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
                    if ignore_not_mounted
                        && (stderr.contains("not found")
                            || stderr.contains("not mounted")
                            || stderr.contains("no such file"))
                    {
                        return Ok(());
                    }
                    tracing::warn!(
                        bin,
                        status = %out.status,
                        stderr = %stderr.trim(),
                        path = %mount_point.display(),
                        "fuse unmount returned non-zero"
                    );
                    last_err = Some(anyhow::anyhow!(
                        "{bin} -u {} failed ({}): {}",
                        mount_point.display(),
                        out.status,
                        stderr.trim()
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("failed to unmount fuse-overlayfs at {}", mount_point.display())
        }))
    }
}

fn find_binary_in_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(name).is_file())
}

impl ViewportDriver for LinuxViewportDriver {
    fn backend_kind(&self) -> CowBackendKind {
        self.backend
    }

    fn materialize_viewport(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        if self.backend == CowBackendKind::LinuxFuseOverlayFs {
            self.materialize_fuse_overlay(trunk_root, viewport_root, cache_dirs)
        } else {
            self.materialize_reflink_or_copy(trunk_root, viewport_root, cache_dirs)
        }
    }

    fn copy_on_write_file(&self, src: &Path, dst: &Path) -> anyhow::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let src_meta = std::fs::metadata(src)?;
        let src_file = File::open(src)?;
        let dst_file = File::create(dst)?;
        match rustix::fs::ioctl_ficlone(&dst_file, &src_file) {
            Ok(()) => {
                let _ = dst_file.set_permissions(src_meta.permissions());
                Ok(())
            }
            Err(errno)
                if errno == rustix::io::Errno::OPNOTSUPP
                    || errno == rustix::io::Errno::NOTSUP
                    || errno == rustix::io::Errno::XDEV
                    || errno == rustix::io::Errno::INVAL =>
            {
                drop(dst_file);
                drop(src_file);
                std::fs::copy(src, dst)?;
                Ok(())
            }
            Err(errno) => {
                drop(dst_file);
                let _ = std::fs::remove_file(dst);
                Err(anyhow::anyhow!(
                    "ioctl_ficlone failed ({} -> {}): {errno}",
                    src.display(),
                    dst.display()
                ))
            }
        }
    }

    fn restore_clean(&self, trunk_root: &Path, viewport_root: &Path) -> anyhow::Result<()> {
        let refs: Vec<&str> = self.default_cache_dirs.iter().map(String::as_str).collect();
        self.restore_clean_with_context(trunk_root, viewport_root, &refs, None)
    }

    fn restore_clean_with_context(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
        mount: Option<&ViewportMount>,
    ) -> anyhow::Result<()> {
        if self.backend == CowBackendKind::LinuxFuseOverlayFs {
            let loom_snapshot = Self::snapshot_loom_dir(&viewport_root.join(".loom"))?;
            let mp = mount
                .and_then(|m| m.mount_point.as_deref())
                .unwrap_or(viewport_root);
            Self::unmount_fuse(mp, true)?;
            let (default_upper, default_work) = Self::overlay_dirs_for(viewport_root);
            let upper = mount
                .and_then(|m| m.upper_dir.as_deref())
                .unwrap_or(&default_upper);
            let work = mount
                .and_then(|m| m.work_dir.as_deref())
                .unwrap_or(&default_work);
            remove_dir_all_safe(upper)?;
            remove_dir_all_safe(work)?;
            self.materialize_fuse_overlay(trunk_root, viewport_root, cache_dirs)?;
            Self::restore_loom_dir(&viewport_root.join(".loom"), loom_snapshot.as_deref())?;
            return Ok(());
        }
        if viewport_root.exists() {
            clean_viewport_for_restore(trunk_root, viewport_root, &[".loom"])?;
        }
        self.materialize_reflink_or_copy(trunk_root, viewport_root, cache_dirs)?;
        Ok(())
    }

    fn teardown_viewport(
        &self,
        viewport_root: &Path,
        mount: Option<&ViewportMount>,
    ) -> anyhow::Result<()> {
        let (default_upper, default_work) = Self::overlay_dirs_for(viewport_root);
        if self.backend == CowBackendKind::LinuxFuseOverlayFs {
            let mp = mount
                .and_then(|m| m.mount_point.as_deref())
                .unwrap_or(viewport_root);
            Self::unmount_fuse(mp, true)?;
            let upper = mount
                .and_then(|m| m.upper_dir.as_deref())
                .unwrap_or(&default_upper);
            let work = mount
                .and_then(|m| m.work_dir.as_deref())
                .unwrap_or(&default_work);
            remove_dir_all_safe(upper)?;
            remove_dir_all_safe(work)?;
        } else {
            if let Some(mp) = mount.and_then(|m| m.mount_point.as_deref()) {
                Self::unmount_fuse(mp, true)?;
            }
            if let Some(upper) = mount.and_then(|m| m.upper_dir.as_deref()) {
                remove_dir_all_safe(upper)?;
            }
            if let Some(work) = mount.and_then(|m| m.work_dir.as_deref()) {
                remove_dir_all_safe(work)?;
            }
        }
        if viewport_root.exists() {
            remove_dir_all_safe(viewport_root)?;
        }
        Ok(())
    }
}
