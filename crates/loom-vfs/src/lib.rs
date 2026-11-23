pub use loom_core::config::VfsDriverPreference;
pub use loom_core::model::{CowBackendKind, ViewportMount, ViewportState};

use std::collections::HashSet;
use std::path::Path;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

pub const DEFAULT_CACHE_DIRS: &[&str] = &["target", "node_modules", ".venv"];

pub trait ViewportDriver: Send + Sync {
    fn backend_kind(&self) -> CowBackendKind;

    fn materialize_viewport(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount>;

    fn copy_on_write_file(&self, src: &Path, dst: &Path) -> anyhow::Result<()>;

    fn restore_clean(&self, trunk_root: &Path, viewport_root: &Path) -> anyhow::Result<()>;

    fn restore_clean_with_context(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        _cache_dirs: &[&str],
        _mount: Option<&ViewportMount>,
    ) -> anyhow::Result<()> {
        self.restore_clean(trunk_root, viewport_root)
    }

    fn teardown_viewport(
        &self,
        viewport_root: &Path,
        mount: Option<&ViewportMount>,
    ) -> anyhow::Result<()>;
}

pub struct GenericCopyDriver;

impl ViewportDriver for GenericCopyDriver {
    fn backend_kind(&self) -> CowBackendKind {
        CowBackendKind::GenericCopyFallback
    }

    fn materialize_viewport(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        std::fs::create_dir_all(viewport_root)?;
        let copied_files = copy_dir_with_cache_isolation(trunk_root, viewport_root, cache_dirs)?;
        Ok(ViewportMount {
            backend: CowBackendKind::GenericCopyFallback,
            viewport_root: viewport_root.to_path_buf(),
            mount_point: None,
            upper_dir: None,
            work_dir: None,
            hardlinked_files: 0,
            copied_files,
            shared_cache_dirs: cache_dirs.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    fn copy_on_write_file(&self, src: &Path, dst: &Path) -> anyhow::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        Ok(())
    }

    fn restore_clean(&self, trunk_root: &Path, viewport_root: &Path) -> anyhow::Result<()> {
        self.restore_clean_with_context(trunk_root, viewport_root, DEFAULT_CACHE_DIRS, None)
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
        std::fs::create_dir_all(viewport_root)?;
        copy_dir_with_cache_isolation(trunk_root, viewport_root, cache_dirs)?;
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

pub fn is_isolated_lockfile(rel_path: &Path) -> bool {
    let Some(name) = rel_path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    name == ".cargo-lock" || name.ends_with(".lock") || name.ends_with(".lck")
}

#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly_recursive(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(path, perms);
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            clear_readonly_recursive(&entry.path())?;
        }
    }
    Ok(())
}

pub fn remove_dir_all_safe(path: &Path) -> std::io::Result<()> {
    if !path.exists() && std::fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly_recursive(path)?;
            std::fs::remove_dir_all(path)
        }
        Err(err) => Err(err),
    }
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn remove_symlink_or_junction(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY) != 0
    {
        return match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(path, perms);
                }
                std::fs::remove_dir(path)
            }
            Err(err) => Err(err),
        };
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            if std::fs::remove_dir(path).is_ok() {
                return Ok(());
            }
            let mut perms = std::fs::symlink_metadata(path)?.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(path, perms);
            }
            std::fs::remove_file(path)
        }
        Err(err) => Err(err),
    }
}

#[cfg(not(windows))]
fn remove_symlink_or_junction(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[allow(clippy::permissions_set_readonly_false)]
fn remove_file_safe(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let mut perms = std::fs::symlink_metadata(path)?.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(path, perms);
            }
            std::fs::remove_file(path)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[allow(clippy::permissions_set_readonly_false)]
fn remove_empty_dir_safe(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let mut perms = std::fs::symlink_metadata(path)?.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(path, perms);
            }
            std::fs::remove_dir(path)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn is_dir_in_use_error(err: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        if matches!(err.raw_os_error(), Some(32 | 33)) {
            return true;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if err.raw_os_error() == Some(rustix::io::Errno::BUSY.raw_os_error()) {
            return true;
        }
    }
    #[cfg(target_os = "macos")]
    {
        if err.raw_os_error() == Some(libc::EBUSY) {
            return true;
        }
    }
    let _ = err;
    false
}

/// Reconciles `viewport_root` in-place prior to re-materializing trunk:
/// - Preserves `keep_names` at the viewport root (e.g. `.loom`).
/// - Unlinks symlinks/junctions safely without touching link targets.
/// - Removes all regular files so trunk files can be cleanly copied/hardlinked/cloned.
/// - Preserves directories that also exist as directories in `trunk_root` (so an open
///   process CWD handle on e.g. `<vp>/src/` never triggers `ERROR_SHARING_VIOLATION`).
/// - Cleans and removes directories not present in `trunk_root`, tolerating an empty
///   non-trunk directory held open as an external process CWD unless trunk expects a file there.
pub fn clean_viewport_for_restore(
    trunk_root: &Path,
    viewport_root: &Path,
    keep_names: &[&str],
) -> std::io::Result<()> {
    let trunk_opt = trunk_root.is_dir().then_some(trunk_root);
    clean_viewport_for_restore_inner(trunk_opt, viewport_root, keep_names, true).map(|_| ())
}

fn dir_contains_only_empty_dirs(dir: &Path) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() || !ft.is_dir() {
            return Ok(false);
        }
        if !dir_contains_only_empty_dirs(&entry.path())? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn clean_viewport_for_restore_inner(
    trunk_dir: Option<&Path>,
    vp_dir: &Path,
    keep_names: &[&str],
    is_root: bool,
) -> std::io::Result<bool> {
    let meta = match std::fs::symlink_metadata(vp_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(false);
    }

    let mut any_kept_in_use = false;

    for entry in std::fs::read_dir(vp_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_str();
        if is_root
            && let Some(ns) = name_str
            && keep_names.contains(&ns)
        {
            continue;
        }
        let vp_path = entry.path();
        let ft = entry.file_type()?;

        if ft.is_symlink() {
            remove_symlink_or_junction(&vp_path)?;
        } else if ft.is_dir() {
            let trunk_child_path = trunk_dir.and_then(|t| {
                if is_root && matches!(name_str, Some(".loom" | ".git")) {
                    None
                } else {
                    Some(t.join(&name))
                }
            });
            let matching_trunk_dir_buf = trunk_child_path.as_deref().and_then(|cand| {
                let is_dir = std::fs::symlink_metadata(cand)
                    .map(|m| !m.file_type().is_symlink() && m.is_dir())
                    .unwrap_or(false);
                if !is_dir {
                    return None;
                }
                #[cfg(windows)]
                {
                    let exact = trunk_dir.and_then(|t| {
                        std::fs::read_dir(t)
                            .ok()
                            .map(|rd| rd.filter_map(Result::ok).any(|e| e.file_name() == name))
                    });
                    if exact != Some(true) {
                        return None;
                    }
                }
                Some(cand.to_path_buf())
            });
            let matching_trunk_dir = matching_trunk_dir_buf.as_deref();

            let child_kept_in_use =
                clean_viewport_for_restore_inner(matching_trunk_dir, &vp_path, keep_names, false)?;
            any_kept_in_use |= child_kept_in_use;

            if matching_trunk_dir.is_none() {
                let trunk_expects_file = trunk_child_path
                    .as_deref()
                    .is_some_and(|cand| cand.is_file());
                if let Err(err) = remove_empty_dir_safe(&vp_path) {
                    let is_empty = std::fs::read_dir(&vp_path)
                        .map(|mut it| it.next().is_none())
                        .unwrap_or(false);
                    let only_empty_dirs = is_empty
                        || (child_kept_in_use
                            && dir_contains_only_empty_dirs(&vp_path).unwrap_or(false));
                    let expected_in_use_err = is_dir_in_use_error(&err)
                        || (child_kept_in_use
                            && err.kind() == std::io::ErrorKind::DirectoryNotEmpty);
                    if !trunk_expects_file && only_empty_dirs && expected_in_use_err {
                        any_kept_in_use = true;
                        tracing::debug!(
                            path = %vp_path.display(),
                            error = %err,
                            "skipping removal of in-use non-trunk directory during restore_clean"
                        );
                    } else {
                        return Err(err);
                    }
                }
            }
        } else {
            remove_file_safe(&vp_path)?;
        }
    }
    Ok(any_kept_in_use)
}

#[allow(clippy::permissions_set_readonly_false)]
pub fn remove_dir_contents_except(dir: &Path, keep_names: &[&str]) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(name_str) = name.to_str()
            && keep_names.contains(&name_str)
        {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            remove_dir_all_safe(&path)?;
        } else if ft.is_symlink() {
            remove_symlink_or_junction(&path)?;
        } else if let Err(err) = std::fs::remove_file(&path) {
            if err.kind() == std::io::ErrorKind::PermissionDenied {
                let mut perms = std::fs::symlink_metadata(&path)?.permissions();
                if perms.readonly() {
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(&path, perms);
                }
                std::fs::remove_file(&path)?;
            } else {
                return Err(err);
            }
        }
    }
    Ok(())
}

pub fn copy_dir_filtered(src: &Path, dst: &Path, skip_names: &[&str]) -> anyhow::Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut count = 0u64;
    if !src.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_name = entry.file_name();
        if let Some(name_str) = file_name.to_str()
            && skip_names.contains(&name_str)
        {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&file_name);
        let ft = entry.file_type()?;
        if ft.is_dir() {
            count += copy_dir_filtered(&src_path, &dst_path, skip_names)?;
        } else if ft.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
            count += 1;
        }
    }
    Ok(count)
}

pub fn copy_dir_with_cache_isolation(
    trunk_root: &Path,
    viewport_root: &Path,
    cache_dirs: &[&str],
) -> anyhow::Result<u64> {
    let cache_set: HashSet<&str> = cache_dirs.iter().copied().collect();
    copy_dir_isolated_inner(trunk_root, trunk_root, viewport_root, &cache_set, false, false)
}

fn copy_dir_isolated_inner(
    trunk_root: &Path,
    src: &Path,
    dst: &Path,
    cache_set: &HashSet<&str>,
    in_cache_dir: bool,
    in_target_dir: bool,
) -> anyhow::Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut count = 0u64;
    if !src.exists() {
        return Ok(0);
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
        let is_root_cache = src == trunk_root
            && name_str.is_some_and(|n| cache_set.contains(n));
        let child_in_cache = in_cache_dir || is_root_cache;
        let child_in_target = in_target_dir || (src == trunk_root && name_str == Some("target"));

        if ft.is_dir() {
            if child_in_target && name_str == Some("incremental") {
                continue;
            }
            count += copy_dir_isolated_inner(
                trunk_root,
                &src_path,
                &dst_path,
                cache_set,
                child_in_cache,
                child_in_target,
            )?;
        } else if ft.is_file() {
            if child_in_cache && is_isolated_lockfile(Path::new(&file_name)) {
                continue;
            }
            std::fs::copy(&src_path, &dst_path)?;
            count += 1;
        }
    }
    Ok(count)
}

pub fn default_driver() -> Box<dyn ViewportDriver> {
    select_driver(VfsDriverPreference::Auto)
}

pub fn detect_driver(repo_root: &Path) -> Box<dyn ViewportDriver> {
    detect_driver_for_path(repo_root)
}

pub fn detect_driver_for_path(repo_root: &Path) -> Box<dyn ViewportDriver> {
    select_driver_for_path(VfsDriverPreference::Auto, repo_root)
}

pub fn select_driver(pref: VfsDriverPreference) -> Box<dyn ViewportDriver> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    select_driver_for_path(pref, &cwd)
}

pub fn select_driver_for_path(
    pref: VfsDriverPreference,
    repo_root: &Path,
) -> Box<dyn ViewportDriver> {
    let default_caches: Vec<String> = DEFAULT_CACHE_DIRS.iter().map(|s| (*s).to_string()).collect();
    select_driver_with_config(pref, repo_root, &default_caches)
}

pub fn select_driver_with_config(
    pref: VfsDriverPreference,
    repo_root: &Path,
    cache_dirs: &[String],
) -> Box<dyn ViewportDriver> {
    #[cfg(not(target_os = "windows"))]
    let _ = (repo_root, cache_dirs);
    match pref {
        VfsDriverPreference::GenericCopyFallback => Box::new(GenericCopyDriver),
        #[cfg(target_os = "windows")]
        VfsDriverPreference::Auto => Box::new(
            windows::WindowsViewportDriver::detect_for_path(repo_root)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "windows")]
        VfsDriverPreference::WindowsRefsBlockClone => Box::new(
            windows::WindowsViewportDriver::with_backend(CowBackendKind::WindowsRefsBlockClone)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "windows")]
        VfsDriverPreference::WindowsProjFs => Box::new(
            windows::WindowsViewportDriver::with_backend(CowBackendKind::WindowsProjFs)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "windows")]
        VfsDriverPreference::WindowsNtfsFallback => Box::new(
            windows::WindowsViewportDriver::with_backend(CowBackendKind::WindowsNtfsFallback)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "linux")]
        VfsDriverPreference::Auto => Box::new(
            linux::LinuxViewportDriver::detect_for_path(repo_root)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "linux")]
        VfsDriverPreference::LinuxReflink => Box::new(
            linux::LinuxViewportDriver::with_backend(CowBackendKind::LinuxReflink)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "linux")]
        VfsDriverPreference::LinuxFuseOverlayFs => Box::new(
            linux::LinuxViewportDriver::with_backend(CowBackendKind::LinuxFuseOverlayFs)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "macos")]
        VfsDriverPreference::Auto => Box::new(
            macos::MacosViewportDriver::detect_for_path(repo_root)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "macos")]
        VfsDriverPreference::ApfsClonefile => Box::new(
            macos::MacosViewportDriver::with_backend(CowBackendKind::ApfsClonefile)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        _ => Box::new(GenericCopyDriver),
    }
}

pub fn driver_for_backend(
    backend: CowBackendKind,
    cache_dirs: &[String],
) -> Box<dyn ViewportDriver> {
    #[cfg(not(target_os = "windows"))]
    let _ = cache_dirs;
    match backend {
        CowBackendKind::GenericCopyFallback => Box::new(GenericCopyDriver),
        #[cfg(target_os = "windows")]
        CowBackendKind::WindowsRefsBlockClone
        | CowBackendKind::WindowsProjFs
        | CowBackendKind::WindowsNtfsFallback => Box::new(
            windows::WindowsViewportDriver::with_backend(backend)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "linux")]
        CowBackendKind::LinuxReflink | CowBackendKind::LinuxFuseOverlayFs => Box::new(
            linux::LinuxViewportDriver::with_backend(backend)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        #[cfg(target_os = "macos")]
        CowBackendKind::ApfsClonefile => Box::new(
            macos::MacosViewportDriver::with_backend(backend)
                .with_cache_dirs(cache_dirs.to_vec()),
        ),
        _ => Box::new(GenericCopyDriver),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_driver_detects_platform_safely() {
        let drv = default_driver();
        #[cfg(target_os = "windows")]
        {
            assert!(matches!(
                drv.backend_kind(),
                CowBackendKind::WindowsNtfsFallback
                    | CowBackendKind::WindowsRefsBlockClone
                    | CowBackendKind::WindowsProjFs
            ));
        }
        #[cfg(target_os = "linux")]
        {
            assert!(matches!(
                drv.backend_kind(),
                CowBackendKind::LinuxReflink
                    | CowBackendKind::LinuxFuseOverlayFs
                    | CowBackendKind::GenericCopyFallback
            ));
        }
        #[cfg(target_os = "macos")]
        {
            assert!(matches!(
                drv.backend_kind(),
                CowBackendKind::ApfsClonefile | CowBackendKind::GenericCopyFallback
            ));
        }
    }

    #[test]
    fn materialize_restore_and_teardown_stub() {
        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp = tmp.path().join("vp");
        std::fs::create_dir_all(trunk.join("src")).unwrap();
        std::fs::create_dir_all(trunk.join(".loom")).unwrap();
        std::fs::create_dir_all(trunk.join(".git")).unwrap();
        std::fs::write(trunk.join("src/lib.rs"), "pub fn hello() {}").unwrap();
        std::fs::write(trunk.join(".loom/secret"), "skip").unwrap();
        std::fs::write(trunk.join(".git/HEAD"), "skip").unwrap();

        let drv = default_driver();
        let mount = drv.materialize_viewport(&trunk, &vp, &["target"]).unwrap();
        assert!(vp.join("src/lib.rs").is_file());
        assert!(!vp.join(".loom/secret").exists());
        assert!(!vp.join(".git/HEAD").exists());
        assert_eq!(mount.copied_files, 1);

        std::fs::write(vp.join("src/lib.rs"), "dirty").unwrap();
        drv.restore_clean(&trunk, &vp).unwrap();
        assert_eq!(
            std::fs::read_to_string(vp.join("src/lib.rs")).unwrap(),
            "pub fn hello() {}"
        );

        drv.teardown_viewport(&vp, Some(&mount)).unwrap();
        assert!(!vp.exists());
    }

    #[test]
    fn isolated_lockfile_detection_covers_cargo_and_dot_lock() {
        assert!(is_isolated_lockfile(Path::new(".cargo-lock")));
        assert!(is_isolated_lockfile(Path::new("target/.cargo-lock")));
        assert!(is_isolated_lockfile(Path::new("foo.lock")));
        assert!(is_isolated_lockfile(Path::new("bar.lck")));
        assert!(!is_isolated_lockfile(Path::new("src/lib.rs")));
        assert!(!is_isolated_lockfile(Path::new("tokio-1234.rlib")));
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn remove_dir_all_safe_handles_readonly_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("ro_dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("sub/locked.txt");
        std::fs::write(&file, "readonly").unwrap();
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).unwrap();

        remove_dir_all_safe(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn restore_clean_replaces_stale_directory_casing() {
        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp = tmp.path().join("vp");
        std::fs::create_dir_all(trunk.join("src/nested")).unwrap();
        std::fs::write(trunk.join("src/nested/lib.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();

        let drv = default_driver();
        let mount = drv.materialize_viewport(&trunk, &vp, &["target"]).unwrap();

        // Rename directories in viewport to mismatched casing (`Src/Nested` via temp names for NTFS)
        let tmp_nested = vp.join("src/nested_tmp");
        std::fs::rename(vp.join("src/nested"), &tmp_nested).unwrap();
        std::fs::rename(&tmp_nested, vp.join("src/Nested")).unwrap();
        let tmp_src = vp.join("src_tmp");
        std::fs::rename(vp.join("src"), &tmp_src).unwrap();
        std::fs::rename(&tmp_src, vp.join("Src")).unwrap();
        std::fs::write(vp.join("Src/Nested/lib.rs"), "dirty").unwrap();

        drv.restore_clean_with_context(&trunk, &vp, &["target"], Some(&mount))
            .unwrap();

        let root_dirs: Vec<String> = std::fs::read_dir(&vp)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            root_dirs.iter().any(|n| n == "src") && !root_dirs.iter().any(|n| n == "Src"),
            "expected exact `src` directory casing after restore_clean, got {root_dirs:?}"
        );
        let sub_dirs: Vec<String> = std::fs::read_dir(vp.join("src"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            sub_dirs.iter().any(|n| n == "nested") && !sub_dirs.iter().any(|n| n == "Nested"),
            "expected exact `nested` directory casing after restore_clean, got {sub_dirs:?}"
        );
        assert_eq!(
            std::fs::read_to_string(vp.join("src/nested/lib.rs")).unwrap(),
            "pub fn ok() -> i32 { 1 }\n"
        );
    }
}
