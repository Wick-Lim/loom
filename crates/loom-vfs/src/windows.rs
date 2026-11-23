use crate::{
    DEFAULT_CACHE_DIRS, ViewportDriver, clean_viewport_for_restore, is_isolated_lockfile,
    remove_dir_all_safe,
};
use loom_core::model::{CowBackendKind, ViewportMount};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION,
    ERROR_MORE_DATA, ERROR_NOT_SUPPORTED, FreeLibrary, GENERIC_READ, GENERIC_WRITE, HANDLE,
    HMODULE, INVALID_HANDLE_VALUE, S_OK,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_SPARSE_FILE,
    FILE_END_OF_FILE_INFO, FILE_SHARE_DELETE, FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileEndOfFileInfo, GetVolumeInformationW, GetVolumePathNameW, OPEN_EXISTING,
    SetFileInformationByHandle,
};
use windows_sys::Win32::Storage::ProjectedFileSystem::{
    PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN,
    PRJ_CB_DATA_FLAG_ENUM_RETURN_SINGLE_ENTRY, PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO,
    PRJ_FLAG_USE_NEGATIVE_PATH_CACHE, PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PRJ_PLACEHOLDER_INFO,
    PRJ_PLACEHOLDER_VERSION_INFO, PRJ_STARTVIRTUALIZING_OPTIONS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_REFS_VOLUME_DATA,
    FSCTL_GET_RETRIEVAL_POINTERS, FSCTL_SET_SPARSE, REFS_VOLUME_DATA_BUFFER,
    RETRIEVAL_POINTERS_BUFFER, STARTING_VCN_INPUT_BUFFER,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING;
use windows_sys::core::{GUID, HRESULT, PCSTR, PCWSTR};

const fn hresult_from_win32(err: u32) -> HRESULT {
    if err == 0 {
        0
    } else {
        ((err & 0x0000_FFFF) | 0x8007_0000) as HRESULT
    }
}

const HRESULT_ERROR_FILE_NOT_FOUND: HRESULT = hresult_from_win32(2);
const HRESULT_ERROR_INSUFFICIENT_BUFFER: HRESULT = hresult_from_win32(122);

fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

unsafe fn wide_ptr_to_string(ptr: PCWSTR) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        OsString::from_wide(slice).to_string_lossy().into_owned()
    }
}

struct HandleGuard(HANDLE);

impl HandleGuard {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// ============================================================================
// Dynamic ProjFS Loader (ZERO static Prj* IAT bindings)
// ============================================================================

type FnPrjMarkDirectoryAsPlaceholder = unsafe extern "system" fn(
    root_path_name: PCWSTR,
    target_path_name: PCWSTR,
    version_info: *const PRJ_PLACEHOLDER_VERSION_INFO,
    virtualization_instance_id: *const GUID,
) -> HRESULT;

type FnPrjStartVirtualizing = unsafe extern "system" fn(
    virtualization_root_path: PCWSTR,
    callbacks: *const PRJ_CALLBACKS,
    instance_context: *const core::ffi::c_void,
    options: *const PRJ_STARTVIRTUALIZING_OPTIONS,
    namespace_virtualization_context: *mut PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
) -> HRESULT;

type FnPrjStopVirtualizing =
    unsafe extern "system" fn(namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);

type FnPrjWritePlaceholderInfo = unsafe extern "system" fn(
    namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    destination_file_name: PCWSTR,
    placeholder_info: *const PRJ_PLACEHOLDER_INFO,
    placeholder_info_size: u32,
) -> HRESULT;

type FnPrjWriteFileData = unsafe extern "system" fn(
    namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    data_stream_id: *const GUID,
    buffer: *const core::ffi::c_void,
    byte_offset: u64,
    length: u32,
) -> HRESULT;

type FnPrjFillDirEntryBuffer = unsafe extern "system" fn(
    file_name: PCWSTR,
    file_basic_info: *const PRJ_FILE_BASIC_INFO,
    dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT;

type FnPrjAllocateAlignedBuffer = unsafe extern "system" fn(
    namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    size: usize,
) -> *mut core::ffi::c_void;

type FnPrjFreeAlignedBuffer = unsafe extern "system" fn(buffer: *const core::ffi::c_void);

type FnPrjFileNameCompare =
    unsafe extern "system" fn(file_name1: PCWSTR, file_name2: PCWSTR) -> i32;

type FnPrjFileNameMatch =
    unsafe extern "system" fn(file_name_to_check: PCWSTR, pattern: PCWSTR) -> u8;

#[derive(Clone, Copy)]
struct ProjFsApi {
    _module: usize,
    mark_directory_as_placeholder: FnPrjMarkDirectoryAsPlaceholder,
    start_virtualizing: FnPrjStartVirtualizing,
    stop_virtualizing: FnPrjStopVirtualizing,
    write_placeholder_info: FnPrjWritePlaceholderInfo,
    write_file_data: FnPrjWriteFileData,
    fill_dir_entry_buffer: FnPrjFillDirEntryBuffer,
    allocate_aligned_buffer: FnPrjAllocateAlignedBuffer,
    free_aligned_buffer: FnPrjFreeAlignedBuffer,
    file_name_compare: Option<FnPrjFileNameCompare>,
    file_name_match: Option<FnPrjFileNameMatch>,
}

static PROJFS_API: OnceLock<Option<ProjFsApi>> = OnceLock::new();

unsafe fn load_proc<T: Copy>(module: HMODULE, name: &[u8]) -> Option<T> {
    let proc = unsafe { GetProcAddress(module, name.as_ptr() as PCSTR)? };
    Some(unsafe { std::mem::transmute_copy(&proc) })
}

impl ProjFsApi {
    fn get() -> Option<&'static Self> {
        PROJFS_API
            .get_or_init(|| unsafe {
                let wide = to_wide_null(OsStr::new("ProjectedFSLib.dll"));
                let module = LoadLibraryW(wide.as_ptr());
                if module.is_null() {
                    return None;
                }
                let Some(mark_directory_as_placeholder) =
                    load_proc(module, b"PrjMarkDirectoryAsPlaceholder\0")
                else {
                    FreeLibrary(module);
                    return None;
                };
                let Some(start_virtualizing) = load_proc(module, b"PrjStartVirtualizing\0") else {
                    FreeLibrary(module);
                    return None;
                };
                let Some(stop_virtualizing) = load_proc(module, b"PrjStopVirtualizing\0") else {
                    FreeLibrary(module);
                    return None;
                };
                let Some(write_placeholder_info) =
                    load_proc(module, b"PrjWritePlaceholderInfo\0")
                else {
                    FreeLibrary(module);
                    return None;
                };
                let Some(write_file_data) = load_proc(module, b"PrjWriteFileData\0") else {
                    FreeLibrary(module);
                    return None;
                };
                let Some(fill_dir_entry_buffer) =
                    load_proc(module, b"PrjFillDirEntryBuffer\0")
                else {
                    FreeLibrary(module);
                    return None;
                };
                let Some(allocate_aligned_buffer) =
                    load_proc(module, b"PrjAllocateAlignedBuffer\0")
                else {
                    FreeLibrary(module);
                    return None;
                };
                let Some(free_aligned_buffer) = load_proc(module, b"PrjFreeAlignedBuffer\0")
                else {
                    FreeLibrary(module);
                    return None;
                };
                let file_name_compare = load_proc(module, b"PrjFileNameCompare\0");
                let file_name_match = load_proc(module, b"PrjFileNameMatch\0");

                Some(ProjFsApi {
                    _module: module as usize,
                    mark_directory_as_placeholder,
                    start_virtualizing,
                    stop_virtualizing,
                    write_placeholder_info,
                    write_file_data,
                    fill_dir_entry_buffer,
                    allocate_aligned_buffer,
                    free_aligned_buffer,
                    file_name_compare,
                    file_name_match,
                })
            })
            .as_ref()
    }
}

struct ProjFsDirEntry {
    name: String,
    is_dir: bool,
    file_size: i64,
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    file_attributes: u32,
}

struct ProjFsEnumState {
    entries: Vec<ProjFsDirEntry>,
    index: usize,
    search_expression: Option<String>,
}

struct ProjFsInstanceState {
    trunk_root: PathBuf,
    cache_dirs: HashSet<String>,
    api: &'static ProjFsApi,
    enumerations: Mutex<HashMap<u128, ProjFsEnumState>>,
}

struct ActiveProjFsInstance {
    ctx_addr: usize,
    state_ptr: usize,
    api: &'static ProjFsApi,
}

fn guid_to_u128(g: &GUID) -> u128 {
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&g.data1.to_le_bytes());
    bytes[4..6].copy_from_slice(&g.data2.to_le_bytes());
    bytes[6..8].copy_from_slice(&g.data3.to_le_bytes());
    bytes[8..16].copy_from_slice(&g.data4);
    u128::from_le_bytes(bytes)
}

fn sanitize_rel_path(rel_str: &str) -> Option<PathBuf> {
    let trimmed = rel_str.trim_start_matches(['\\', '/']);
    let mut out = PathBuf::new();
    for part in trimmed.split(['\\', '/']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

fn should_skip_rel_path(rel: &Path, cache_dirs: &HashSet<String>) -> bool {
    let mut comps = rel.components();
    let Some(first) = comps.next() else {
        return false;
    };
    let first_str = first.as_os_str().to_string_lossy();
    if first_str == ".loom" || first_str == ".git" {
        return true;
    }
    if cache_dirs.contains(first_str.as_ref()) {
        if first_str == "target"
            && rel
                .components()
                .any(|c| c.as_os_str() == OsStr::new("incremental"))
        {
            return true;
        }
        if is_isolated_lockfile(rel) {
            return true;
        }
    }
    false
}

fn metadata_to_basic_info(meta: &std::fs::Metadata) -> PRJ_FILE_BASIC_INFO {
    PRJ_FILE_BASIC_INFO {
        IsDirectory: meta.is_dir(),
        FileSize: if meta.is_dir() { 0 } else { meta.len() as i64 },
        CreationTime: meta.creation_time() as i64,
        LastAccessTime: meta.last_access_time() as i64,
        LastWriteTime: meta.last_write_time() as i64,
        ChangeTime: meta.last_write_time() as i64,
        FileAttributes: meta.file_attributes(),
    }
}

unsafe fn get_instance_state<'a>(
    callback_data: *const PRJ_CALLBACK_DATA,
) -> Option<&'a ProjFsInstanceState> {
    if callback_data.is_null() {
        return None;
    }
    let ctx = unsafe { (*callback_data).InstanceContext };
    if ctx.is_null() {
        return None;
    }
    Some(unsafe { &*(ctx as *const ProjFsInstanceState) })
}

unsafe extern "system" fn prj_start_dir_enum_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let Some(state) = get_instance_state(callback_data) else {
            return E_INVALIDARG;
        };
        if enumeration_id.is_null() {
            return E_INVALIDARG;
        }
        let rel_raw = wide_ptr_to_string((*callback_data).FilePathName);
        let Some(rel) = sanitize_rel_path(&rel_raw) else {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        };
        if should_skip_rel_path(&rel, &state.cache_dirs) {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        }
        let dir_path = state.trunk_root.join(&rel);
        let Ok(read_dir) = std::fs::read_dir(&dir_path) else {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        };

        let mut entries = Vec::new();
        for entry in read_dir.flatten() {
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            let entry_rel = rel.join(name_str);
            if should_skip_rel_path(&entry_rel, &state.cache_dirs) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            entries.push(ProjFsDirEntry {
                name: name_str.to_string(),
                is_dir: meta.is_dir(),
                file_size: if meta.is_dir() { 0 } else { meta.len() as i64 },
                creation_time: meta.creation_time() as i64,
                last_access_time: meta.last_access_time() as i64,
                last_write_time: meta.last_write_time() as i64,
                file_attributes: meta.file_attributes(),
            });
        }

        if let Some(cmp_fn) = state.api.file_name_compare {
            entries.sort_by(|a, b| {
                let wa = to_wide_null(OsStr::new(&a.name));
                let wb = to_wide_null(OsStr::new(&b.name));
                let ord = cmp_fn(wa.as_ptr(), wb.as_ptr());
                ord.cmp(&0)
            });
        } else {
            entries.sort_by_key(|a| a.name.to_lowercase());
        }

        let key = guid_to_u128(&*enumeration_id);
        let Ok(mut guard) = state.enumerations.lock() else {
            return E_FAIL;
        };
        guard.insert(
            key,
            ProjFsEnumState {
                entries,
                index: 0,
                search_expression: None,
            },
        );
        S_OK
    }))
    .unwrap_or(E_FAIL)
}

unsafe extern "system" fn prj_end_dir_enum_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let Some(state) = get_instance_state(callback_data) else {
            return E_INVALIDARG;
        };
        if enumeration_id.is_null() {
            return E_INVALIDARG;
        }
        let key = guid_to_u128(&*enumeration_id);
        if let Ok(mut guard) = state.enumerations.lock() {
            guard.remove(&key);
        }
        S_OK
    }))
    .unwrap_or(E_FAIL)
}

unsafe extern "system" fn prj_get_dir_enum_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
    search_expression: PCWSTR,
    dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let Some(state) = get_instance_state(callback_data) else {
            return E_INVALIDARG;
        };
        if enumeration_id.is_null() {
            return E_INVALIDARG;
        }
        let key = guid_to_u128(&*enumeration_id);
        let Ok(mut guard) = state.enumerations.lock() else {
            return E_FAIL;
        };
        let Some(enum_state) = guard.get_mut(&key) else {
            return E_INVALIDARG;
        };

        let flags = (*callback_data).Flags;
        if (flags & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN) != 0 {
            enum_state.index = 0;
            enum_state.search_expression = if search_expression.is_null() {
                None
            } else {
                let s = wide_ptr_to_string(search_expression);
                if s.is_empty() { None } else { Some(s) }
            };
        } else if enum_state.search_expression.is_none() && !search_expression.is_null() {
            let s = wide_ptr_to_string(search_expression);
            if !s.is_empty() {
                enum_state.search_expression = Some(s);
            }
        }

        let single = (flags & PRJ_CB_DATA_FLAG_ENUM_RETURN_SINGLE_ENTRY) != 0;
        let pattern_wide = enum_state
            .search_expression
            .as_deref()
            .map(|p| to_wide_null(OsStr::new(p)));

        while enum_state.index < enum_state.entries.len() {
            let item = &enum_state.entries[enum_state.index];
            let name_wide = to_wide_null(OsStr::new(&item.name));
            if let (Some(pat), Some(match_fn)) = (&pattern_wide, state.api.file_name_match)
                && match_fn(name_wide.as_ptr(), pat.as_ptr()) == 0
            {
                enum_state.index += 1;
                continue;
            }
            let basic = PRJ_FILE_BASIC_INFO {
                IsDirectory: item.is_dir,
                FileSize: item.file_size,
                CreationTime: item.creation_time,
                LastAccessTime: item.last_access_time,
                LastWriteTime: item.last_write_time,
                ChangeTime: item.last_write_time,
                FileAttributes: item.file_attributes,
            };
            let hr = (state.api.fill_dir_entry_buffer)(
                name_wide.as_ptr(),
                &basic,
                dir_entry_buffer_handle,
            );
            if hr == HRESULT_ERROR_INSUFFICIENT_BUFFER {
                break;
            }
            if hr < 0 {
                return hr;
            }
            enum_state.index += 1;
            if single {
                break;
            }
        }
        S_OK
    }))
    .unwrap_or(E_FAIL)
}

unsafe extern "system" fn prj_get_placeholder_info_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
) -> HRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let Some(state) = get_instance_state(callback_data) else {
            return E_INVALIDARG;
        };
        let rel_raw = wide_ptr_to_string((*callback_data).FilePathName);
        let Some(rel) = sanitize_rel_path(&rel_raw) else {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        };
        if should_skip_rel_path(&rel, &state.cache_dirs) {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        }
        let full_path = state.trunk_root.join(&rel);
        let Ok(meta) = std::fs::metadata(&full_path) else {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        };

        let mut placeholder = PRJ_PLACEHOLDER_INFO {
            FileBasicInfo: metadata_to_basic_info(&meta),
            ..Default::default()
        };
        let rel_bytes = rel_raw.as_bytes();
        let copy_len = rel_bytes.len().min(placeholder.VersionInfo.ContentID.len());
        placeholder.VersionInfo.ContentID[..copy_len].copy_from_slice(&rel_bytes[..copy_len]);

        (state.api.write_placeholder_info)(
            (*callback_data).NamespaceVirtualizationContext,
            (*callback_data).FilePathName,
            &placeholder,
            std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
        )
    }))
    .unwrap_or(E_FAIL)
}

unsafe extern "system" fn prj_get_file_data_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        use std::io::{Read, Seek, SeekFrom};
        let Some(state) = get_instance_state(callback_data) else {
            return E_INVALIDARG;
        };
        let rel_raw = wide_ptr_to_string((*callback_data).FilePathName);
        let Some(rel) = sanitize_rel_path(&rel_raw) else {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        };
        if should_skip_rel_path(&rel, &state.cache_dirs) {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        }
        let full_path = state.trunk_root.join(&rel);
        let Ok(mut file) = File::open(&full_path) else {
            return HRESULT_ERROR_FILE_NOT_FOUND;
        };
        if file.seek(SeekFrom::Start(byte_offset)).is_err() {
            return E_FAIL;
        }

        let chunk_cap = (length as usize).clamp(4096, 1024 * 1024);
        let buf_ptr = (state.api.allocate_aligned_buffer)(
            (*callback_data).NamespaceVirtualizationContext,
            chunk_cap,
        );
        if buf_ptr.is_null() {
            return E_OUTOFMEMORY;
        }

        let mut remaining = length as usize;
        let mut current_offset = byte_offset;
        let mut result_hr = S_OK;

        while remaining > 0 {
            let to_read = remaining.min(chunk_cap);
            let slice = std::slice::from_raw_parts_mut(buf_ptr as *mut u8, to_read);
            let mut filled = 0usize;
            while filled < to_read {
                match file.read(&mut slice[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => {
                        result_hr = E_FAIL;
                        break;
                    }
                }
            }
            if result_hr < 0 || filled == 0 {
                break;
            }
            let hr = (state.api.write_file_data)(
                (*callback_data).NamespaceVirtualizationContext,
                &(*callback_data).DataStreamId,
                buf_ptr,
                current_offset,
                filled as u32,
            );
            if hr < 0 {
                result_hr = hr;
                break;
            }
            current_offset += filled as u64;
            remaining -= filled;
        }

        (state.api.free_aligned_buffer)(buf_ptr);
        result_hr
    }))
    .unwrap_or(E_FAIL)
}

// ============================================================================
// WindowsViewportDriver
// ============================================================================

static PROJFS_INSTANCES: OnceLock<Mutex<HashMap<PathBuf, ActiveProjFsInstance>>> = OnceLock::new();

fn projfs_instances_map() -> &'static Mutex<HashMap<PathBuf, ActiveProjFsInstance>> {
    PROJFS_INSTANCES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lexical_normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut prefix_or_root: Vec<Component<'_>> = Vec::new();
    let mut normals: Vec<Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                prefix_or_root.push(comp);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normals.is_empty() {
                    normals.pop();
                } else if prefix_or_root.is_empty() {
                    normals.push(comp);
                }
            }
            Component::Normal(_) => normals.push(comp),
        }
    }
    let mut out = PathBuf::new();
    for c in prefix_or_root.into_iter().chain(normals) {
        out.push(c.as_os_str());
    }
    out
}

pub struct WindowsViewportDriver {
    backend: CowBackendKind,
    default_cache_dirs: Vec<String>,
}

impl WindowsViewportDriver {
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
        if Self::supports_refs_block_clone(repo_root) {
            return CowBackendKind::WindowsRefsBlockClone;
        }
        if Self::probe_projfs_dll_available() {
            return CowBackendKind::WindowsProjFs;
        }
        CowBackendKind::WindowsNtfsFallback
    }

    /// Probes ProjectedFSLib.dll dynamically via LoadLibraryW + GetProcAddress.
    /// NEVER statically binds or calls windows_sys::...::Prj* functions.
    pub fn probe_projfs_dll_available() -> bool {
        ProjFsApi::get().is_some()
    }

    pub fn resolve_volume_root(path: &Path) -> Option<String> {
        let existing = path.ancestors().find(|p| !p.as_os_str().is_empty() && p.exists());
        let target = match existing {
            Some(p) => std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()),
            None => {
                if path.is_relative() && !path.has_root() {
                    std::env::current_dir().ok()?
                } else {
                    return None;
                }
            }
        };
        let wide_in = to_wide_null(target.as_os_str());
        let mut buf = vec![0u16; 1024];
        let ok = unsafe {
            GetVolumePathNameW(wide_in.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
        };
        if ok == 0 {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let mut s = OsString::from_wide(&buf[..end])
            .to_string_lossy()
            .into_owned();
        if !s.ends_with('\\') {
            s.push('\\');
        }
        Some(s)
    }

    pub fn supports_refs_block_clone(path: &Path) -> bool {
        let Some(vol_root) = Self::resolve_volume_root(path) else {
            return false;
        };
        let wide = to_wide_null(OsStr::new(&vol_root));
        let mut flags: u32 = 0;
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        ok != 0 && (flags & FILE_SUPPORTS_BLOCK_REFCOUNTING) != 0
    }

    fn normalize_instance_key(path: &Path) -> PathBuf {
        if let Ok(canon) = std::fs::canonicalize(path) {
            return canon;
        }
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(path)
        } else {
            path.to_path_buf()
        };
        let lex = lexical_normalize_path(&abs);
        let mut tail: Vec<OsString> = Vec::new();
        let mut cur: Option<&Path> = Some(lex.as_path());
        while let Some(p) = cur {
            if p.exists()
                && let Ok(canon) = std::fs::canonicalize(p)
            {
                let mut out = canon;
                for comp in tail.into_iter().rev() {
                    out.push(comp);
                }
                return out;
            }
            if let Some(name) = p.file_name() {
                tail.push(name.to_os_string());
                cur = p.parent();
            } else {
                break;
            }
        }
        lex
    }

    fn stop_projfs_if_active(&self, viewport_root: &Path) {
        let key = Self::normalize_instance_key(viewport_root);
        let lex_key = lexical_normalize_path(viewport_root);
        let active = {
            let Ok(mut guard) = projfs_instances_map().lock() else {
                return;
            };
            guard
                .remove(&key)
                .or_else(|| guard.remove(&lex_key))
                .or_else(|| guard.remove(viewport_root))
        };
        if let Some(inst) = active {
            unsafe {
                (inst.api.stop_virtualizing)(
                    inst.ctx_addr as PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
                );
                let _ = Box::from_raw(inst.state_ptr as *mut ProjFsInstanceState);
            }
        }
    }

    fn materialize_inner(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        match self.backend {
            CowBackendKind::WindowsRefsBlockClone => {
                self.materialize_refs_block_clone(trunk_root, viewport_root, cache_dirs)
            }
            CowBackendKind::WindowsProjFs => {
                self.materialize_projfs(trunk_root, viewport_root, cache_dirs)
            }
            CowBackendKind::WindowsNtfsFallback | CowBackendKind::GenericCopyFallback => {
                self.materialize_ntfs_fallback(trunk_root, viewport_root, cache_dirs)
            }
            _ => self.materialize_ntfs_fallback(trunk_root, viewport_root, cache_dirs),
        }
    }

    // ------------------------------------------------------------------------
    // ReFS Block Clone Implementation
    // ------------------------------------------------------------------------

    fn materialize_refs_block_clone(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        std::fs::create_dir_all(viewport_root)?;
        let cache_set: HashSet<&str> = cache_dirs.iter().copied().collect();
        let mut cloned_or_copied = 0u64;
        self.walk_refs_clone(
            trunk_root,
            trunk_root,
            viewport_root,
            &cache_set,
            false,
            false,
            &mut cloned_or_copied,
        )?;
        Ok(ViewportMount {
            backend: CowBackendKind::WindowsRefsBlockClone,
            viewport_root: viewport_root.to_path_buf(),
            mount_point: None,
            upper_dir: None,
            work_dir: None,
            hardlinked_files: 0,
            copied_files: cloned_or_copied,
            shared_cache_dirs: cache_dirs.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_refs_clone(
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
                self.walk_refs_clone(
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
                Self::refs_block_clone_file(&src_path, &dst_path)?;
                *copied_files += 1;
            }
        }
        Ok(())
    }

    fn refs_block_clone_file(src: &Path, dst: &Path) -> anyhow::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let dst_parent = dst.parent().unwrap_or(dst);
        if Self::resolve_volume_root(src) != Self::resolve_volume_root(dst_parent)
            || !Self::supports_refs_block_clone(src)
        {
            std::fs::copy(src, dst)?;
            return Ok(());
        }

        let meta = std::fs::metadata(src)?;
        let file_len = meta.len();
        if file_len == 0 {
            File::create(dst)?;
            return Ok(());
        }

        let src_wide = to_wide_null(src.as_os_str());
        let dst_wide = to_wide_null(dst.as_os_str());

        let src_raw = unsafe {
            CreateFileW(
                src_wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if src_raw == INVALID_HANDLE_VALUE || src_raw.is_null() {
            std::fs::copy(src, dst)?;
            return Ok(());
        }
        let src_handle = HandleGuard(src_raw);

        let mut vol_data = REFS_VOLUME_DATA_BUFFER::default();
        let mut bytes_returned: u32 = 0;
        let ok_vol = unsafe {
            DeviceIoControl(
                src_handle.raw(),
                FSCTL_GET_REFS_VOLUME_DATA,
                std::ptr::null(),
                0,
                (&raw mut vol_data).cast(),
                std::mem::size_of::<REFS_VOLUME_DATA_BUFFER>() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };
        if ok_vol == 0 || vol_data.BytesPerCluster == 0 {
            let err = std::io::Error::last_os_error();
            let raw = err.raw_os_error().unwrap_or(0) as u32;
            if raw == ERROR_INVALID_FUNCTION || raw == ERROR_NOT_SUPPORTED {
                drop(src_handle);
                std::fs::copy(src, dst)?;
                return Ok(());
            }
            drop(src_handle);
            std::fs::copy(src, dst)?;
            return Ok(());
        }
        let cluster_size = vol_data.BytesPerCluster as i64;

        // Collect retrieval pointers
        let mut extents: Vec<(i64, i64, i64)> = Vec::new();
        let mut next_vcn_start = 0i64;
        let mut buf_u64 = vec![0u64; 512];
        loop {
            let input = STARTING_VCN_INPUT_BUFFER {
                StartingVcn: next_vcn_start,
            };
            let mut returned = 0u32;
            let ok_rp = unsafe {
                DeviceIoControl(
                    src_handle.raw(),
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    (&raw const input).cast(),
                    std::mem::size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                    buf_u64.as_mut_ptr().cast(),
                    (buf_u64.len() * std::mem::size_of::<u64>()) as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            let last_err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
            if ok_rp == 0 && last_err == ERROR_MORE_DATA {
                // Parse current partial extents or grow buffer and retry
                if buf_u64.len() < 16384 {
                    buf_u64.resize(buf_u64.len() * 2, 0);
                    continue;
                }
            }
            if ok_rp == 0 && last_err == ERROR_HANDLE_EOF {
                break;
            }
            if ok_rp == 0 && last_err != ERROR_MORE_DATA {
                drop(src_handle);
                std::fs::copy(src, dst)?;
                return Ok(());
            }
            let rp_ptr = buf_u64.as_ptr() as *const RETRIEVAL_POINTERS_BUFFER;
            let extent_count = unsafe { (*rp_ptr).ExtentCount as usize };
            if extent_count == 0 {
                break;
            }
            let mut cur_vcn = unsafe { (*rp_ptr).StartingVcn };
            let ext_slice =
                unsafe { std::slice::from_raw_parts((*rp_ptr).Extents.as_ptr(), extent_count) };
            for ext in ext_slice {
                extents.push((cur_vcn, ext.NextVcn, ext.Lcn));
                cur_vcn = ext.NextVcn;
            }
            next_vcn_start = cur_vcn;
            if ok_rp != 0 {
                break;
            }
        }

        if extents.is_empty() {
            // Resident small file or no clusters
            drop(src_handle);
            std::fs::copy(src, dst)?;
            return Ok(());
        }

        let dst_raw = unsafe {
            CreateFileW(
                dst_wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_NONE,
                std::ptr::null(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if dst_raw == INVALID_HANDLE_VALUE || dst_raw.is_null() {
            drop(src_handle);
            std::fs::copy(src, dst)?;
            return Ok(());
        }
        let dst_handle = HandleGuard(dst_raw);

        if (meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE) != 0 {
            let mut br = 0u32;
            let _ = unsafe {
                DeviceIoControl(
                    dst_handle.raw(),
                    FSCTL_SET_SPARSE,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut br,
                    std::ptr::null_mut(),
                )
            };
        }

        let aligned_len = (((file_len as i64) + cluster_size - 1) / cluster_size) * cluster_size;
        let eof_aligned = FILE_END_OF_FILE_INFO {
            EndOfFile: aligned_len,
        };
        let ok_ext = unsafe {
            SetFileInformationByHandle(
                dst_handle.raw(),
                FileEndOfFileInfo,
                (&raw const eof_aligned).cast(),
                std::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
            )
        };
        if ok_ext == 0 {
            drop(dst_handle);
            drop(src_handle);
            std::fs::copy(src, dst)?;
            return Ok(());
        }

        for (start_vcn, next_vcn, lcn) in extents {
            if lcn == -1 {
                continue;
            }
            let offset = start_vcn * cluster_size;
            let byte_count = (next_vcn - start_vcn) * cluster_size;
            if byte_count <= 0 {
                continue;
            }
            let dup = DUPLICATE_EXTENTS_DATA {
                FileHandle: src_handle.raw(),
                SourceFileOffset: offset,
                TargetFileOffset: offset,
                ByteCount: byte_count,
            };
            let mut br = 0u32;
            let ok_dup = unsafe {
                DeviceIoControl(
                    dst_handle.raw(),
                    FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                    (&raw const dup).cast(),
                    std::mem::size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut br,
                    std::ptr::null_mut(),
                )
            };
            if ok_dup == 0 {
                drop(dst_handle);
                drop(src_handle);
                std::fs::copy(src, dst)?;
                return Ok(());
            }
        }

        let eof_exact = FILE_END_OF_FILE_INFO {
            EndOfFile: file_len as i64,
        };
        let ok_final = unsafe {
            SetFileInformationByHandle(
                dst_handle.raw(),
                FileEndOfFileInfo,
                (&raw const eof_exact).cast(),
                std::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
            )
        };
        if ok_final == 0 {
            drop(dst_handle);
            drop(src_handle);
            std::fs::copy(src, dst)?;
            return Ok(());
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Dynamic ProjFS Implementation
    // ------------------------------------------------------------------------

    fn materialize_projfs(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        let api = ProjFsApi::get().ok_or_else(|| {
            anyhow::anyhow!("ProjectedFSLib.dll is not available on this Windows host")
        })?;
        std::fs::create_dir_all(viewport_root)?;
        self.stop_projfs_if_active(viewport_root);

        let id = uuid::Uuid::new_v4();
        let (d1, d2, d3, d4) = id.to_fields_le();
        let instance_guid = GUID {
            data1: d1,
            data2: d2,
            data3: d3,
            data4: *d4,
        };
        let vp_wide = to_wide_null(viewport_root.as_os_str());
        let hr_mark = unsafe {
            (api.mark_directory_as_placeholder)(
                vp_wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                &instance_guid,
            )
        };
        if hr_mark < 0 {
            anyhow::bail!(
                "PrjMarkDirectoryAsPlaceholder failed with HRESULT 0x{:08x}",
                hr_mark as u32
            );
        }

        let cache_set: HashSet<String> = cache_dirs.iter().map(|s| (*s).to_string()).collect();
        let state = Box::new(ProjFsInstanceState {
            trunk_root: trunk_root.to_path_buf(),
            cache_dirs: cache_set,
            api,
            enumerations: Mutex::new(HashMap::new()),
        });
        let state_ptr = Box::into_raw(state);

        let callbacks = PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(prj_start_dir_enum_cb),
            EndDirectoryEnumerationCallback: Some(prj_end_dir_enum_cb),
            GetDirectoryEnumerationCallback: Some(prj_get_dir_enum_cb),
            GetPlaceholderInfoCallback: Some(prj_get_placeholder_info_cb),
            GetFileDataCallback: Some(prj_get_file_data_cb),
            QueryFileNameCallback: None,
            NotificationCallback: None,
            CancelCommandCallback: None,
        };
        let options = PRJ_STARTVIRTUALIZING_OPTIONS {
            Flags: PRJ_FLAG_USE_NEGATIVE_PATH_CACHE,
            PoolThreadCount: 0,
            ConcurrentThreadCount: 0,
            NotificationMappings: std::ptr::null_mut(),
            NotificationMappingsCount: 0,
        };

        let mut ns_ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT = std::ptr::null_mut();
        let hr_start = unsafe {
            (api.start_virtualizing)(
                vp_wide.as_ptr(),
                &callbacks,
                state_ptr.cast(),
                &options,
                &mut ns_ctx,
            )
        };
        if hr_start < 0 {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            anyhow::bail!(
                "PrjStartVirtualizing failed with HRESULT 0x{:08x}",
                hr_start as u32
            );
        }

        let key = Self::normalize_instance_key(viewport_root);
        if let Ok(mut guard) = projfs_instances_map().lock() {
            guard.insert(
                key,
                ActiveProjFsInstance {
                    ctx_addr: ns_ctx as usize,
                    state_ptr: state_ptr as usize,
                    api,
                },
            );
        }

        Ok(ViewportMount {
            backend: CowBackendKind::WindowsProjFs,
            viewport_root: viewport_root.to_path_buf(),
            mount_point: Some(viewport_root.to_path_buf()),
            upper_dir: None,
            work_dir: None,
            hardlinked_files: 0,
            copied_files: 0,
            shared_cache_dirs: cache_dirs.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    // ------------------------------------------------------------------------
    // NTFS Directory/Hardlink Fallback Implementation
    // ------------------------------------------------------------------------

    fn materialize_ntfs_fallback(
        &self,
        trunk_root: &Path,
        viewport_root: &Path,
        cache_dirs: &[&str],
    ) -> anyhow::Result<ViewportMount> {
        std::fs::create_dir_all(viewport_root)?;
        let ws_crates = parse_workspace_crate_names(trunk_root);
        let cache_set: HashSet<&str> = cache_dirs.iter().copied().collect();
        let mut hardlinked_files = 0u64;
        let mut copied_files = 0u64;

        Self::walk_ntfs_trunk(
            trunk_root,
            viewport_root,
            &cache_set,
            &ws_crates,
            &mut hardlinked_files,
            &mut copied_files,
        )?;

        Ok(ViewportMount {
            backend: self.backend,
            viewport_root: viewport_root.to_path_buf(),
            mount_point: None,
            upper_dir: None,
            work_dir: None,
            hardlinked_files,
            copied_files,
            shared_cache_dirs: cache_dirs.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    fn walk_ntfs_trunk(
        trunk_root: &Path,
        viewport_root: &Path,
        cache_set: &HashSet<&str>,
        ws_crates: &HashSet<String>,
        hardlinked_files: &mut u64,
        copied_files: &mut u64,
    ) -> anyhow::Result<()> {
        if !trunk_root.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(trunk_root)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name_str = file_name.to_str();
            if matches!(name_str, Some(".loom" | ".git")) {
                continue;
            }
            let src_path = entry.path();
            let dst_path = viewport_root.join(&file_name);
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if let Some(name) = name_str
                    && cache_set.contains(name)
                {
                    if name == "target" {
                        Self::materialize_target_dir(
                            &src_path,
                            &dst_path,
                            ws_crates,
                            hardlinked_files,
                            copied_files,
                        )?;
                    } else {
                        Self::materialize_immutable_cache_dir(
                            &src_path,
                            &dst_path,
                            hardlinked_files,
                            copied_files,
                        )?;
                    }
                } else {
                    Self::copy_editable_dir(&src_path, &dst_path, copied_files)?;
                }
            } else if ft.is_file() {
                std::fs::copy(&src_path, &dst_path)?;
                *copied_files += 1;
            }
        }
        Ok(())
    }

    fn copy_editable_dir(
        src: &Path,
        dst: &Path,
        copied_files: &mut u64,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let src_path = entry.path();
            let dst_path = dst.join(&file_name);
            let ft = entry.file_type()?;
            if ft.is_dir() {
                Self::copy_editable_dir(&src_path, &dst_path, copied_files)?;
            } else if ft.is_file() {
                std::fs::copy(&src_path, &dst_path)?;
                *copied_files += 1;
            }
        }
        Ok(())
    }

    fn hardlink_or_copy(
        src: &Path,
        dst: &Path,
        hardlinked_files: &mut u64,
        copied_files: &mut u64,
    ) -> anyhow::Result<()> {
        if dst.exists() {
            let _ = std::fs::remove_file(dst);
        }
        match std::fs::hard_link(src, dst) {
            Ok(()) => {
                *hardlinked_files += 1;
            }
            Err(_) => {
                std::fs::copy(src, dst)?;
                *copied_files += 1;
            }
        }
        Ok(())
    }

    fn materialize_immutable_cache_dir(
        src: &Path,
        dst: &Path,
        hardlinked_files: &mut u64,
        copied_files: &mut u64,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let src_path = entry.path();
            let dst_path = dst.join(&file_name);
            let ft = entry.file_type()?;
            if ft.is_dir() {
                Self::materialize_immutable_cache_dir(
                    &src_path,
                    &dst_path,
                    hardlinked_files,
                    copied_files,
                )?;
            } else if ft.is_file() {
                if is_isolated_lockfile(Path::new(&file_name)) {
                    continue;
                }
                Self::hardlink_or_copy(&src_path, &dst_path, hardlinked_files, copied_files)?;
            }
        }
        Ok(())
    }

    fn materialize_target_dir(
        src: &Path,
        dst: &Path,
        ws_crates: &HashSet<String>,
        hardlinked_files: &mut u64,
        copied_files: &mut u64,
    ) -> anyhow::Result<()> {
        Self::walk_target_dir(src, dst, None, ws_crates, hardlinked_files, copied_files)
    }

    fn walk_target_dir(
        src: &Path,
        dst: &Path,
        section: Option<&str>,
        ws_crates: &HashSet<String>,
        hardlinked_files: &mut u64,
        copied_files: &mut u64,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            let src_path = entry.path();
            let dst_path = dst.join(&file_name);
            let ft = entry.file_type()?;

            if ft.is_dir() {
                if name_str == "incremental" {
                    continue;
                }
                match section {
                    Some("build") | Some(".fingerprint") => {
                        if matches_workspace_crate(&name_str, ws_crates) {
                            Self::copy_editable_dir(&src_path, &dst_path, copied_files)?;
                        } else {
                            Self::materialize_immutable_cache_dir(
                                &src_path,
                                &dst_path,
                                hardlinked_files,
                                copied_files,
                            )?;
                        }
                    }
                    _ => {
                        let next_section = match name_str.as_ref() {
                            "deps" => Some("deps"),
                            "build" => Some("build"),
                            ".fingerprint" => Some(".fingerprint"),
                            _ => section,
                        };
                        Self::walk_target_dir(
                            &src_path,
                            &dst_path,
                            next_section,
                            ws_crates,
                            hardlinked_files,
                            copied_files,
                        )?;
                    }
                }
            } else if ft.is_file() {
                if is_isolated_lockfile(Path::new(&file_name)) {
                    continue;
                }
                if section == Some("deps")
                    && is_immutable_deps_artifact(&src_path)
                    && !matches_workspace_crate(&name_str, ws_crates)
                {
                    Self::hardlink_or_copy(&src_path, &dst_path, hardlinked_files, copied_files)?;
                } else {
                    std::fs::copy(&src_path, &dst_path)?;
                    *copied_files += 1;
                }
            }
        }
        Ok(())
    }
}

fn is_immutable_deps_artifact(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rlib" | "rmeta" | "dll" | "d" | "lib" | "so" | "dylib"
    )
}

fn matches_workspace_crate(name: &str, ws_crates: &HashSet<String>) -> bool {
    let trimmed = name.strip_prefix("lib").unwrap_or(name);
    for candidate in [name, trimmed] {
        for ws in ws_crates {
            if let Some(rest) = candidate.strip_prefix(ws.as_str())
                && (rest.is_empty() || rest.starts_with('-') || rest.starts_with('.'))
            {
                return true;
            }
        }
    }
    false
}

fn insert_crate_name(set: &mut HashSet<String>, name: &str) {
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        set.insert(trimmed.to_string());
        set.insert(trimmed.replace('-', "_"));
    }
}

fn parse_cargo_toml_names(cargo_toml: &Path, out: &mut HashSet<String>) {
    let Ok(text) = std::fs::read_to_string(cargo_toml) else {
        return;
    };
    let Ok(val) = toml::from_str::<toml::Value>(&text) else {
        return;
    };
    if let Some(name) = val
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        insert_crate_name(out, name);
    }
    if let Some(name) = val
        .get("lib")
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
    {
        insert_crate_name(out, name);
    }
    if let Some(bins) = val.get("bin").and_then(|b| b.as_array()) {
        for bin in bins {
            if let Some(name) = bin.get("name").and_then(|n| n.as_str()) {
                insert_crate_name(out, name);
            }
        }
    }
}

fn parse_workspace_crate_names(trunk_root: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let root_toml = trunk_root.join("Cargo.toml");
    parse_cargo_toml_names(&root_toml, &mut out);

    let Ok(text) = std::fs::read_to_string(&root_toml) else {
        return out;
    };
    let Ok(val) = toml::from_str::<toml::Value>(&text) else {
        return out;
    };
    if let Some(members) = val
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for member in members {
            let Some(pattern) = member.as_str() else {
                continue;
            };
            if let Some(prefix) = pattern.strip_suffix("/*") {
                let parent = trunk_root.join(prefix);
                if let Ok(entries) = std::fs::read_dir(&parent) {
                    for entry in entries.flatten() {
                        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                            parse_cargo_toml_names(&entry.path().join("Cargo.toml"), &mut out);
                        }
                    }
                }
            } else if let Some(prefix) = pattern.strip_suffix("\\*") {
                let parent = trunk_root.join(prefix);
                if let Ok(entries) = std::fs::read_dir(&parent) {
                    for entry in entries.flatten() {
                        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                            parse_cargo_toml_names(&entry.path().join("Cargo.toml"), &mut out);
                        }
                    }
                }
            } else {
                parse_cargo_toml_names(&trunk_root.join(pattern).join("Cargo.toml"), &mut out);
            }
        }
    }
    out
}

impl ViewportDriver for WindowsViewportDriver {
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
        if self.backend == CowBackendKind::WindowsRefsBlockClone {
            return Self::refs_block_clone_file(src, dst);
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
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
        self.stop_projfs_if_active(viewport_root);
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
        self.stop_projfs_if_active(viewport_root);
        if viewport_root.exists() {
            remove_dir_all_safe(viewport_root)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use tempfile::TempDir;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    fn get_file_link_and_id(path: &Path) -> (u32, u64) {
        let wide = to_wide_null(path.as_os_str());
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        assert!(!raw.is_null());
        let guard = HandleGuard(raw);
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(guard.raw(), &mut info) };
        assert_ne!(ok, 0);
        let id = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
        let _keep: *mut c_void = guard.raw();
        (info.nNumberOfLinks, id)
    }

    #[test]
    fn windows_driver_detects_ntfs_without_projfs_crash() {
        let drv = WindowsViewportDriver::detect();
        assert_eq!(drv.backend_kind(), CowBackendKind::WindowsNtfsFallback);
    }

    #[test]
    fn volume_probe_handles_relative_and_missing_child_paths() {
        let cwd = std::env::current_dir().unwrap();
        let vol = WindowsViewportDriver::resolve_volume_root(&cwd);
        assert!(vol.is_some());
        assert!(vol.unwrap().ends_with('\\'));

        let tmp = TempDir::new().unwrap();
        let non_existent_child = tmp.path().join("future_repo").join(".loom/viewports/vp1");
        let child_vol = WindowsViewportDriver::resolve_volume_root(&non_existent_child);
        assert!(child_vol.is_some());

        let drv = crate::select_driver_for_path(
            loom_core::config::VfsDriverPreference::Auto,
            &non_existent_child,
        );
        assert!(matches!(
            drv.backend_kind(),
            CowBackendKind::WindowsNtfsFallback
                | CowBackendKind::WindowsRefsBlockClone
                | CowBackendKind::WindowsProjFs
        ));
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn ntfs_fallback_isolates_editable_files_and_hardlinks_immutable_caches() {
        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp_a = tmp.path().join("vp_a");
        let vp_b = tmp.path().join("vp_b");

        std::fs::create_dir_all(trunk.join("src")).unwrap();
        std::fs::create_dir_all(trunk.join("target/debug/deps")).unwrap();
        std::fs::create_dir_all(trunk.join("target/debug/incremental/inc_item")).unwrap();
        std::fs::create_dir_all(trunk.join("target/debug/.fingerprint/tokio-1111")).unwrap();
        std::fs::create_dir_all(trunk.join("target/debug/.fingerprint/my-app-2222")).unwrap();
        std::fs::create_dir_all(trunk.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(trunk.join(".venv/lib")).unwrap();
        std::fs::create_dir_all(trunk.join(".loom")).unwrap();
        std::fs::create_dir_all(trunk.join(".git")).unwrap();

        std::fs::write(
            trunk.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(trunk.join("Cargo.lock"), "lock-content").unwrap();
        std::fs::write(trunk.join("src/lib.rs"), "pub fn v() -> i32 { 1 }").unwrap();
        std::fs::write(trunk.join("target/.cargo-lock"), "cargo-lock-root").unwrap();
        std::fs::write(trunk.join("target/debug/.cargo-lock"), "cargo-lock-debug").unwrap();
        std::fs::write(
            trunk.join("target/debug/incremental/inc_item/state.bin"),
            "inc",
        )
        .unwrap();
        std::fs::write(
            trunk.join("target/debug/deps/tokio-1234.rlib"),
            "third-party-rlib",
        )
        .unwrap();
        std::fs::write(
            trunk.join("target/debug/deps/my_app-5678.rlib"),
            "workspace-rlib",
        )
        .unwrap();
        std::fs::write(trunk.join("target/debug/my_app.exe"), "exe-bytes").unwrap();
        std::fs::write(
            trunk.join("target/debug/.fingerprint/tokio-1111/dep-lib"),
            "fp-tokio",
        )
        .unwrap();
        std::fs::write(
            trunk.join("target/debug/.fingerprint/my-app-2222/dep-bin"),
            "fp-myapp",
        )
        .unwrap();
        std::fs::write(trunk.join("node_modules/pkg/index.js"), "module.exports = 1;").unwrap();
        std::fs::write(trunk.join("node_modules/.yarn.lock"), "skip-lock").unwrap();
        std::fs::write(trunk.join(".venv/lib/site.py"), "x = 1").unwrap();
        std::fs::write(trunk.join(".loom/state.redb"), "secret").unwrap();
        std::fs::write(trunk.join(".git/HEAD"), "ref: refs/heads/main").unwrap();

        let drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsNtfsFallback);
        let mount_a = drv
            .materialize_viewport(&trunk, &vp_a, &["target", "node_modules", ".venv"])
            .unwrap();
        let mount_b = drv
            .materialize_viewport(&trunk, &vp_b, &["target", "node_modules", ".venv"])
            .unwrap();

        assert_eq!(mount_a.backend, CowBackendKind::WindowsNtfsFallback);
        assert!(mount_a.hardlinked_files >= 4);
        assert!(mount_b.hardlinked_files >= 4);

        // .loom, .git, .cargo-lock, *.lock inside cache_dirs, and incremental/ must NOT be copied
        assert!(!vp_a.join(".loom/state.redb").exists());
        assert!(!vp_a.join(".git/HEAD").exists());
        assert!(!vp_a.join("target/.cargo-lock").exists());
        assert!(!vp_a.join("target/debug/.cargo-lock").exists());
        assert!(!vp_a.join("target/debug/incremental").exists());
        assert!(!vp_a.join("node_modules/.yarn.lock").exists());

        // Editable Cargo.lock at repo root MUST be copied independently
        assert!(vp_a.join("Cargo.lock").is_file());
        let (links_lock, id_lock_vp) = get_file_link_and_id(&vp_a.join("Cargo.lock"));
        let (_, id_lock_trunk) = get_file_link_and_id(&trunk.join("Cargo.lock"));
        assert_eq!(links_lock, 1);
        assert_ne!(id_lock_vp, id_lock_trunk);

        // Mutating src/lib.rs in viewport A must NOT mutate trunk or viewport B
        std::fs::write(vp_a.join("src/lib.rs"), "pub fn v() -> i32 { 99 }").unwrap();
        assert_eq!(
            std::fs::read_to_string(trunk.join("src/lib.rs")).unwrap(),
            "pub fn v() -> i32 { 1 }"
        );
        assert_eq!(
            std::fs::read_to_string(vp_b.join("src/lib.rs")).unwrap(),
            "pub fn v() -> i32 { 1 }"
        );

        // Workspace crate artifacts must be independent copies (nNumberOfLinks == 1)
        let (ws_links, ws_id_vp) =
            get_file_link_and_id(&vp_a.join("target/debug/deps/my_app-5678.rlib"));
        let (_, ws_id_trunk) =
            get_file_link_and_id(&trunk.join("target/debug/deps/my_app-5678.rlib"));
        assert_eq!(ws_links, 1);
        assert_ne!(ws_id_vp, ws_id_trunk);

        let (exe_links, _) = get_file_link_and_id(&vp_a.join("target/debug/my_app.exe"));
        assert_eq!(exe_links, 1);

        let (fp_ws_links, _) =
            get_file_link_and_id(&vp_a.join("target/debug/.fingerprint/my-app-2222/dep-bin"));
        assert_eq!(fp_ws_links, 1);

        // Third-party deps, node_modules, .venv, third-party fingerprint must be hardlinked
        let (dep_links, dep_id_vp) =
            get_file_link_and_id(&vp_a.join("target/debug/deps/tokio-1234.rlib"));
        let (_, dep_id_trunk) =
            get_file_link_and_id(&trunk.join("target/debug/deps/tokio-1234.rlib"));
        assert!(dep_links >= 3);
        assert_eq!(dep_id_vp, dep_id_trunk);

        let (nm_links, nm_id_vp) = get_file_link_and_id(&vp_a.join("node_modules/pkg/index.js"));
        let (_, nm_id_trunk) = get_file_link_and_id(&trunk.join("node_modules/pkg/index.js"));
        assert!(nm_links >= 3);
        assert_eq!(nm_id_vp, nm_id_trunk);

        let (venv_links, venv_id_vp) = get_file_link_and_id(&vp_a.join(".venv/lib/site.py"));
        let (_, venv_id_trunk) = get_file_link_and_id(&trunk.join(".venv/lib/site.py"));
        assert!(venv_links >= 3);
        assert_eq!(venv_id_vp, venv_id_trunk);

        // restore_clean preserves <vp>/.loom/RADAR_STATUS.json, restores src/lib.rs, removes untracked
        std::fs::create_dir_all(vp_a.join(".loom")).unwrap();
        std::fs::write(vp_a.join(".loom/RADAR_STATUS.json"), "{\"status\":\"GREEN\"}").unwrap();
        std::fs::write(vp_a.join("src/untracked.rs"), "temp").unwrap();
        drv.restore_clean(&trunk, &vp_a).unwrap();
        assert_eq!(
            std::fs::read_to_string(vp_a.join(".loom/RADAR_STATUS.json")).unwrap(),
            "{\"status\":\"GREEN\"}"
        );
        assert_eq!(
            std::fs::read_to_string(vp_a.join("src/lib.rs")).unwrap(),
            "pub fn v() -> i32 { 1 }"
        );
        assert!(!vp_a.join("src/untracked.rs").exists());
        let (_, dep_id_after) =
            get_file_link_and_id(&vp_a.join("target/debug/deps/tokio-1234.rlib"));
        assert_eq!(dep_id_after, dep_id_trunk);

        // teardown_viewport succeeds even if a file in viewport is marked readonly
        let mut perms = std::fs::metadata(vp_a.join("src/lib.rs"))
            .unwrap()
            .permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(vp_a.join("src/lib.rs"), perms).unwrap();
        drv.teardown_viewport(&vp_a, Some(&mount_a)).unwrap();
        assert!(!vp_a.exists());

        drv.teardown_viewport(&vp_b, Some(&mount_b)).unwrap();
        assert!(!vp_b.exists());
    }

    #[test]
    fn refs_block_clone_falls_back_cleanly_on_ntfs_and_workspace_globs_work() {
        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("ws");
        std::fs::create_dir_all(trunk.join("crates/my-lib")).unwrap();
        std::fs::write(
            trunk.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            trunk.join("crates/my-lib/Cargo.toml"),
            "[package]\nname = \"my-lib\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let ws_crates = parse_workspace_crate_names(&trunk);
        assert!(matches_workspace_crate("my_lib-abcd1234.rlib", &ws_crates));
        assert!(matches_workspace_crate("my-lib-abcd1234", &ws_crates));
        assert!(matches_workspace_crate("libmy_lib-abcd1234.rlib", &ws_crates));
        assert!(!matches_workspace_crate(
            "my_lib_extra-abcd1234.rlib",
            &ws_crates
        ));

        // ReFS block clone on NTFS volume should cleanly copy 0-byte and non-empty files
        let src_empty = tmp.path().join("empty.bin");
        let dst_empty = tmp.path().join("empty_copy.bin");
        std::fs::write(&src_empty, b"").unwrap();
        WindowsViewportDriver::refs_block_clone_file(&src_empty, &dst_empty).unwrap();
        assert_eq!(std::fs::read(&dst_empty).unwrap(), b"");

        let src_data = tmp.path().join("data.bin");
        let dst_data = tmp.path().join("data_copy.bin");
        std::fs::write(&src_data, b"refs fallback test payload").unwrap();
        WindowsViewportDriver::refs_block_clone_file(&src_data, &dst_data).unwrap();
        assert_eq!(
            std::fs::read(&dst_data).unwrap(),
            b"refs fallback test payload"
        );

        // Explicit WindowsProjFs on host without ProjectedFSLib.dll returns clean error (no crash)
        if !WindowsViewportDriver::probe_projfs_dll_available() {
            let proj_drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsProjFs);
            let res = proj_drv.materialize_viewport(&trunk, &tmp.path().join("vp_proj"), &["target"]);
            assert!(res.is_err());
        }
    }

    #[test]
    fn resolve_volume_root_returns_none_for_missing_rooted_drive() {
        let missing = ('Q'..='Z')
            .chain('D'..='P')
            .map(|d| format!("{d}:\\definitely_missing_drive_xyz\\repo"))
            .find(|p| !Path::new(&p[..3]).exists())
            .expect("expected at least one unused Windows drive letter");
        assert_eq!(
            WindowsViewportDriver::resolve_volume_root(Path::new(&missing)),
            None
        );
        if !Path::new(r"Q:\").exists() {
            assert_eq!(
                WindowsViewportDriver::resolve_volume_root(Path::new(
                    r"Q:\definitely_missing_drive_xyz\repo"
                )),
                None
            );
        }
    }

    #[test]
    fn ntfs_fallback_isolates_custom_lib_name_rlib() {
        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp_a = tmp.path().join("vp_a");
        std::fs::create_dir_all(trunk.join("crates/ws-custom-pkg/src")).unwrap();
        std::fs::create_dir_all(trunk.join("target/debug/deps")).unwrap();

        std::fs::write(
            trunk.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            trunk.join("crates/ws-custom-pkg/Cargo.toml"),
            "[package]\nname = \"ws-custom-pkg\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"custom_abi_lib\"\n",
        )
        .unwrap();
        std::fs::write(
            trunk.join("crates/ws-custom-pkg/src/lib.rs"),
            "pub fn abi() -> u32 { 7 }\n",
        )
        .unwrap();
        std::fs::write(
            trunk.join("target/debug/deps/libcustom_abi_lib-9999.rlib"),
            "custom-lib-rlib",
        )
        .unwrap();
        std::fs::write(
            trunk.join("target/debug/deps/third_party-1111.rlib"),
            "third-party-rlib",
        )
        .unwrap();

        let drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsNtfsFallback);
        drv.materialize_viewport(&trunk, &vp_a, &["target"]).unwrap();

        let (custom_links, custom_vp_id) =
            get_file_link_and_id(&vp_a.join("target/debug/deps/libcustom_abi_lib-9999.rlib"));
        let (_, custom_trunk_id) =
            get_file_link_and_id(&trunk.join("target/debug/deps/libcustom_abi_lib-9999.rlib"));
        assert_eq!(custom_links, 1, "custom [lib].name .rlib must be copied, not hardlinked");
        assert_ne!(custom_vp_id, custom_trunk_id);

        let (third_links, third_vp_id) =
            get_file_link_and_id(&vp_a.join("target/debug/deps/third_party-1111.rlib"));
        let (_, third_trunk_id) =
            get_file_link_and_id(&trunk.join("target/debug/deps/third_party-1111.rlib"));
        assert_eq!(third_links, 2);
        assert_eq!(third_vp_id, third_trunk_id);

        drv.teardown_viewport(&vp_a, None).unwrap();
    }

    #[test]
    fn restore_clean_unlinks_windows_directory_junction_without_touching_target() {
        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp = tmp.path().join("vp");
        let junc_target = tmp.path().join("external_junction_target");

        std::fs::create_dir_all(trunk.join("src")).unwrap();
        std::fs::write(trunk.join("src/lib.rs"), "pub fn clean() {}\n").unwrap();
        std::fs::create_dir_all(&junc_target).unwrap();
        std::fs::write(junc_target.join("sentinel.txt"), "do-not-delete").unwrap();

        let drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsNtfsFallback);
        drv.materialize_viewport(&trunk, &vp, &["target"]).unwrap();
        std::fs::create_dir_all(vp.join(".loom")).unwrap();
        std::fs::write(vp.join(".loom/RADAR_STATUS.json"), "{\"status\":\"GREEN\"}").unwrap();

        let junc_link = vp.join("linked_dir");
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&junc_link)
            .arg(&junc_target)
            .output()
            .expect("run mklink /J");
        assert!(
            status.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(junc_link.join("sentinel.txt").is_file());

        drv.restore_clean(&trunk, &vp).unwrap();

        assert!(std::fs::symlink_metadata(&junc_link).is_err());
        assert_eq!(
            std::fs::read_to_string(junc_target.join("sentinel.txt")).unwrap(),
            "do-not-delete"
        );
        assert_eq!(
            std::fs::read_to_string(vp.join(".loom/RADAR_STATUS.json")).unwrap(),
            "{\"status\":\"GREEN\"}"
        );
        assert_eq!(
            std::fs::read_to_string(vp.join("src/lib.rs")).unwrap(),
            "pub fn clean() {}\n"
        );

        drv.teardown_viewport(&vp, None).unwrap();
    }

    #[test]
    fn restore_clean_succeeds_when_subdir_handle_lacks_share_delete() {
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp = tmp.path().join("vp");
        std::fs::create_dir_all(trunk.join("src")).unwrap();
        std::fs::write(trunk.join("src/lib.rs"), "pub fn clean() -> i32 { 1 }\n").unwrap();
        std::fs::write(trunk.join("README.md"), "trunk readme\n").unwrap();

        let drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsNtfsFallback);
        drv.materialize_viewport(&trunk, &vp, &["target"]).unwrap();
        std::fs::create_dir_all(vp.join(".loom")).unwrap();
        std::fs::write(vp.join(".loom/RADAR_STATUS.json"), "{\"status\":\"GREEN\"}").unwrap();
        std::fs::write(vp.join("src/lib.rs"), "dirty\n").unwrap();
        std::fs::write(vp.join("src/extra.rs"), "extra\n").unwrap();
        std::fs::create_dir_all(vp.join("extra_dir")).unwrap();
        std::fs::write(vp.join("extra_dir/temp.txt"), "temp\n").unwrap();

        // Hold an open directory handle on <vp>/src WITHOUT FILE_SHARE_DELETE,
        // matching Windows process CWD semantics.
        let src_dir = vp.join("src");
        let wide = to_wide_null(src_dir.as_os_str());
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        assert!(!raw.is_null());
        let _cwd_handle_guard = HandleGuard(raw);

        drv.restore_clean(&trunk, &vp).unwrap();

        assert_eq!(
            std::fs::read_to_string(vp.join("src/lib.rs")).unwrap(),
            "pub fn clean() -> i32 { 1 }\n"
        );
        assert_eq!(
            std::fs::read_to_string(vp.join("README.md")).unwrap(),
            "trunk readme\n"
        );
        assert!(!vp.join("src/extra.rs").exists());
        assert!(!vp.join("extra_dir").exists());
        assert_eq!(
            std::fs::read_to_string(vp.join(".loom/RADAR_STATUS.json")).unwrap(),
            "{\"status\":\"GREEN\"}"
        );

        drop(_cwd_handle_guard);
        drv.teardown_viewport(&vp, None).unwrap();
    }

    #[test]
    fn restore_clean_succeeds_when_nested_2level_nontrunk_dir_handle_lacks_share_delete() {
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp = tmp.path().join("vp");
        std::fs::create_dir_all(trunk.join("src")).unwrap();
        std::fs::write(trunk.join("src/lib.rs"), "pub fn clean() -> i32 { 1 }\n").unwrap();

        let drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsNtfsFallback);
        drv.materialize_viewport(&trunk, &vp, &["target"]).unwrap();
        std::fs::create_dir_all(vp.join(".loom")).unwrap();
        std::fs::write(vp.join(".loom/RADAR_STATUS.json"), "{\"status\":\"GREEN\"}").unwrap();

        let nested = vp.join("tmp_extra").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("scratch.txt"), "dirty\n").unwrap();
        std::fs::write(vp.join("tmp_extra/sibling.txt"), "dirty\n").unwrap();

        let wide = to_wide_null(nested.as_os_str());
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        assert!(!raw.is_null());
        let _cwd_handle_guard = HandleGuard(raw);

        drv.restore_clean(&trunk, &vp).unwrap();

        assert!(!nested.join("scratch.txt").exists());
        assert!(!vp.join("tmp_extra/sibling.txt").exists());
        assert_eq!(
            std::fs::read_to_string(vp.join("src/lib.rs")).unwrap(),
            "pub fn clean() -> i32 { 1 }\n"
        );
        assert_eq!(
            std::fs::read_to_string(vp.join(".loom/RADAR_STATUS.json")).unwrap(),
            "{\"status\":\"GREEN\"}"
        );

        drop(_cwd_handle_guard);
        drv.teardown_viewport(&vp, None).unwrap();
    }

    #[test]
    fn restore_clean_succeeds_when_parent_and_child_nontrunk_dir_handles_both_lack_share_delete() {
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

        let tmp = TempDir::new().unwrap();
        let trunk = tmp.path().join("trunk");
        let vp = tmp.path().join("vp");
        std::fs::create_dir_all(trunk.join("src")).unwrap();
        std::fs::write(trunk.join("src/lib.rs"), "pub fn clean() -> i32 { 1 }\n").unwrap();

        let drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsNtfsFallback);
        drv.materialize_viewport(&trunk, &vp, &["target"]).unwrap();

        let parent_extra = vp.join("tmp_extra");
        let nested = parent_extra.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("scratch.txt"), "dirty\n").unwrap();

        let open_no_share_delete = |dir: &Path| -> HandleGuard {
            let wide = to_wide_null(dir.as_os_str());
            let raw = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(raw, INVALID_HANDLE_VALUE);
            assert!(!raw.is_null());
            HandleGuard(raw)
        };

        let _parent_guard = open_no_share_delete(&parent_extra);
        let _child_guard = open_no_share_delete(&nested);

        drv.restore_clean(&trunk, &vp).unwrap();
        assert!(!nested.join("scratch.txt").exists());
        assert_eq!(
            std::fs::read_to_string(vp.join("src/lib.rs")).unwrap(),
            "pub fn clean() -> i32 { 1 }\n"
        );

        drop(_child_guard);
        drop(_parent_guard);
        drv.teardown_viewport(&vp, None).unwrap();
    }

    #[test]
    fn projfs_registry_key_normalization_survives_deleted_dir_and_cross_driver_instances() {
        let tmp = TempDir::new().unwrap();
        let vp = tmp.path().join("vp_projfs_key");
        std::fs::create_dir_all(&vp).unwrap();
        let key_existing = WindowsViewportDriver::normalize_instance_key(&vp);
        let key_with_dots =
            WindowsViewportDriver::normalize_instance_key(&tmp.path().join("sub/../vp_projfs_key/."));
        assert_eq!(key_existing, key_with_dots);

        std::fs::remove_dir_all(&vp).unwrap();
        let key_deleted = WindowsViewportDriver::normalize_instance_key(&vp);
        assert_eq!(key_existing, key_deleted);

        // Process-global registry is shared across separate WindowsViewportDriver instances
        let api = ProjFsApi {
            _module: 0,
            mark_directory_as_placeholder: prj_stub_mark,
            start_virtualizing: prj_stub_start,
            stop_virtualizing: prj_stub_stop,
            write_placeholder_info: prj_stub_write_ph,
            write_file_data: prj_stub_write_data,
            fill_dir_entry_buffer: prj_stub_fill_dir,
            allocate_aligned_buffer: prj_stub_alloc,
            free_aligned_buffer: prj_stub_free,
            file_name_compare: None,
            file_name_match: None,
        };
        let api_ref: &'static ProjFsApi = Box::leak(Box::new(api));
        let state = Box::new(ProjFsInstanceState {
            trunk_root: tmp.path().to_path_buf(),
            cache_dirs: HashSet::new(),
            api: api_ref,
            enumerations: Mutex::new(HashMap::new()),
        });
        let state_ptr = Box::into_raw(state) as usize;
        {
            let mut guard = projfs_instances_map().lock().unwrap();
            guard.insert(
                key_existing.clone(),
                ActiveProjFsInstance {
                    ctx_addr: 0x1234,
                    state_ptr,
                    api: api_ref,
                },
            );
        }

        // Calling stop_projfs_if_active on a brand-new driver instance must find and remove it
        let fresh_drv = WindowsViewportDriver::with_backend(CowBackendKind::WindowsProjFs);
        fresh_drv.stop_projfs_if_active(&vp);
        assert!(
            !projfs_instances_map()
                .lock()
                .unwrap()
                .contains_key(&key_existing)
        );
    }

    unsafe extern "system" fn prj_stub_mark(
        _: PCWSTR,
        _: PCWSTR,
        _: *const PRJ_PLACEHOLDER_VERSION_INFO,
        _: *const GUID,
    ) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn prj_stub_start(
        _: PCWSTR,
        _: *const PRJ_CALLBACKS,
        _: *const core::ffi::c_void,
        _: *const PRJ_STARTVIRTUALIZING_OPTIONS,
        _: *mut PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    ) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn prj_stub_stop(ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT) {
        assert_eq!(ctx as usize, 0x1234);
    }
    unsafe extern "system" fn prj_stub_write_ph(
        _: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        _: PCWSTR,
        _: *const PRJ_PLACEHOLDER_INFO,
        _: u32,
    ) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn prj_stub_write_data(
        _: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        _: *const GUID,
        _: *const core::ffi::c_void,
        _: u64,
        _: u32,
    ) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn prj_stub_fill_dir(
        _: PCWSTR,
        _: *const PRJ_FILE_BASIC_INFO,
        _: PRJ_DIR_ENTRY_BUFFER_HANDLE,
    ) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn prj_stub_alloc(
        _: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        _: usize,
    ) -> *mut core::ffi::c_void {
        std::ptr::null_mut()
    }
    unsafe extern "system" fn prj_stub_free(_: *const core::ffi::c_void) {}
}
