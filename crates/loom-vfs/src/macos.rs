use crate::{
    DEFAULT_CACHE_DIRS, ViewportDriver, clean_viewport_for_restore, is_isolated_lockfile,
    remove_dir_all_safe,
};
use loom_core::model::{CowBackendKind, ViewportMount};
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub struct MacosViewportDriver {
    backend: CowBackendKind,
    default_cache_dirs: Vec<String>,
}

impl MacosViewportDriver {
    pub fn new() -> Self {
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
        let Ok(c_path) = CString::new(probe_target.as_os_str().as_bytes()) else {
            return CowBackendKind::GenericCopyFallback;
        };
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
        if rc == 0 {
            let fstype = unsafe { CStr::from_ptr(buf.f_fstypename.as_ptr()) }.to_string_lossy();
            if fstype.eq_ignore_ascii_case("apfs") {
                return CowBackendKind::ApfsClonefile;
            }
        }
        CowBackendKind::GenericCopyFallback
    }

    fn materialize_inner(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        std::fs::create_dir_all(viewport_root)?;
        let cache_set: HashSet<&str> = cache_dirs.iter().copied().collect();
        let mut copied_files = 0u64;
        self.walk_clone(
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
    fn walk_clone(
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
                self.walk_clone(
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
                if self.backend == CowBackendKind::ApfsClonefile {
                    self.copy_on_write_file(&src_path, &dst_path)?;
                } else {
                    std::fs::copy(&src_path, &dst_path)?;
                }
                *copied_files += 1;
            }
        }
        Ok(())
    }
}

impl Default for MacosViewportDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewportDriver for MacosViewportDriver {
    fn backend_kind(&self) -> CowBackendKind {
        self.backend
    }

    fn materialize_viewport(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        self.materialize_inner(trunk_root, viewport_root, cache_dirs)
    }

    fn copy_on_write_file(&self, src: &Path, dst: &Path) -> anyhow::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dst.symlink_metadata().is_ok() {
            std::fs::remove_file(dst)?;
        }
        let c_src = CString::new(src.as_os_str().as_bytes())?;
        let c_dst = CString::new(dst.as_os_str().as_bytes())?;
        let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if matches!(err.raw_os_error(), Some(libc::EXDEV) | Some(libc::ENOTSUP)) {
                std::fs::copy(src, dst)?;
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
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
        _mount: Option<&ViewportMount>,
    ) -> anyhow::Result<()> {
        if viewport_root.exists() {
            clean_viewport_for_restore(trunk_root, viewport_root, &[".loom"])?;
        }
        self.materialize_inner(trunk_root, viewport_root, cache_dirs)?;
        Ok(())
    }

    fn teardown_viewport(
        &self,
        viewport_root: &Path,
        _mount: Option<&ViewportMount>,
    ) -> anyhow::Result<()> {
        if viewport_root.exists() {
            remove_dir_all_safe(viewport_root)?;
        }
        Ok(())
    }
}
