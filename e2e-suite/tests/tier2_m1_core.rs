use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use e2e_suite::{CommandOutput, TestRepo};
use serde_json::Value;

const STATUS_DLL_NOT_FOUND_EXIT: i32 = -1073741515; // 0xC0000135
const STATUS_ACCESS_VIOLATION_EXIT: i32 = -1073741819; // 0xC0000005

#[track_caller]
fn assert_no_crash_signatures(out: &CommandOutput) {
    assert_ne!(
        out.exit_code(),
        Some(STATUS_DLL_NOT_FOUND_EXIT),
        "binary crashed with STATUS_DLL_NOT_FOUND (0xC0000135)\nstderr:\n{}",
        out.stderr_str()
    );
    assert_ne!(
        out.exit_code(),
        Some(STATUS_ACCESS_VIOLATION_EXIT),
        "binary crashed with STATUS_ACCESS_VIOLATION (0xC0000005)\nstderr:\n{}",
        out.stderr_str()
    );
    let combined = format!("{}\n{}", out.stdout_str(), out.stderr_str());
    assert!(
        !combined.contains("panicked at")
            && !combined.contains("RUST_BACKTRACE")
            && !combined.contains("stack backtrace:")
            && !combined.contains("STATUS_DLL_NOT_FOUND"),
        "unexpected panic or crash signature in output:\n{combined}"
    );
}

#[track_caller]
fn assert_clean_failure(out: &CommandOutput) {
    out.assert_failure();
    assert!(
        out.exit_code().is_some(),
        "process terminated without numeric exit code\nstderr:\n{}",
        out.stderr_str()
    );
    assert!(
        !out.stderr_str().trim().is_empty() || !out.stdout_str().trim().is_empty(),
        "expected diagnostic message on failure, got empty stdout/stderr"
    );
    assert_no_crash_signatures(out);
}

fn kill_pid(pid: u32) {
    if pid == 0 || pid == std::process::id() {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn kill_daemon(repo: &TestRepo) -> Option<u32> {
    let pid = repo.daemon_pid()?;
    kill_pid(pid);
    std::thread::sleep(Duration::from_millis(50));
    Some(pid)
}

fn write_bytes_retry(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::write(path, bytes) {
            Ok(()) => return,
            Err(e)
                if Instant::now() < deadline
                    && (e.kind() == ErrorKind::PermissionDenied
                        || matches!(e.raw_os_error(), Some(32 | 33))) =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("failed to write {}: {e}", path.display()),
        }
    }
}

fn remove_file_retry(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::remove_file(path) {
            Ok(()) => return,
            Err(e) if e.kind() == ErrorKind::NotFound => return,
            Err(e)
                if Instant::now() < deadline
                    && (e.kind() == ErrorKind::PermissionDenied
                        || matches!(e.raw_os_error(), Some(32 | 33))) =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("failed to remove {}: {e}", path.display()),
        }
    }
}

fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                collect_files_recursive(&p, out);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
}

fn find_cas_blobs(root: &Path) -> Vec<PathBuf> {
    let cas_dir = root.join(".loom").join("cas");
    let mut files = Vec::new();
    collect_files_recursive(&cas_dir, &mut files);
    let zstd_blobs: Vec<PathBuf> = files
        .iter()
        .filter(|p| {
            fs::read(p)
                .map(|b| b.len() >= 4 && b[..4] == [0x28, 0xB5, 0x2F, 0xFD])
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if !zstd_blobs.is_empty() {
        zstd_blobs
    } else {
        files
            .into_iter()
            .filter(|p| fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false))
            .collect()
    }
}

fn find_wal_segments(root: &Path) -> Vec<PathBuf> {
    let wal_dir = root.join(".loom").join("wal");
    let mut files = Vec::new();
    collect_files_recursive(&wal_dir, &mut files);
    files
        .into_iter()
        .filter(|p| {
            fs::read(p)
                .map(|b| b.len() >= 4 && &b[..4] == b"LMWA")
                .unwrap_or(false)
        })
        .collect()
}

fn find_second_wal_frame_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 60 || &bytes[..4] != b"LMWA" {
        return None;
    }
    let payload_len = u32::from_le_bytes(bytes[24..28].try_into().ok()?) as usize;
    let frame1_off = 60usize.checked_add(payload_len)?;
    if frame1_off + 4 <= bytes.len() && &bytes[frame1_off..frame1_off + 4] == b"LMWA" {
        return Some(frame1_off);
    }
    bytes[60..]
        .windows(4)
        .position(|w| w == b"LMWA")
        .map(|idx| 60 + idx)
}

fn has_colocate_git_enabled(val: &toml::Value) -> bool {
    let Some(table) = val.as_table() else {
        return false;
    };
    for (k, v) in table {
        if k == "colocate_git" && v.as_bool() == Some(true) {
            return true;
        }
        if v.is_table() && has_colocate_git_enabled(v) {
            return true;
        }
    }
    false
}

fn ensure_multi_frame_wal_segment(repo: &TestRepo) -> (PathBuf, Vec<u8>, usize) {
    for _ in 0..3 {
        repo.loom(&["gc"]).assert_success();
        let segments = find_wal_segments(repo.root());
        if let Some(found) = segments.iter().find_map(|p| {
            let bytes = fs::read(p).ok()?;
            find_second_wal_frame_offset(&bytes).map(|off| (p.clone(), bytes, off))
        }) {
            return found;
        }
    }
    panic!("expected at least one WAL segment with >= 2 frames after init + gc");
}

fn mutate_idle_timeout_in_toml(val: &mut toml::Value, new_value: i64) -> bool {
    let Some(table) = val.as_table_mut() else {
        return false;
    };
    let mut updated = false;
    for (k, v) in table.iter_mut() {
        let lower = k.to_ascii_lowercase();
        if lower.contains("idle") && lower.contains("timeout") && v.is_integer() {
            *v = toml::Value::Integer(new_value);
            updated = true;
        } else if v.is_table() && mutate_idle_timeout_in_toml(v, new_value) {
            updated = true;
        }
    }
    updated
}

fn set_idle_timeout_in_config(config_path: &Path, new_value: i64) {
    let raw = fs::read_to_string(config_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", config_path.display()));
    let mut val: toml::Value = toml::from_str(&raw)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", config_path.display()));
    if !mutate_idle_timeout_in_toml(&mut val, new_value) {
        if let Some(table) = val.as_table_mut() {
            table.insert(
                "idle_timeout_secs".to_string(),
                toml::Value::Integer(new_value),
            );
        }
    }
    let serialized = toml::to_string(&val).expect("serialize mutated config.toml");
    write_bytes_retry(config_path, serialized.as_bytes());
}

fn spawn_and_wait_dead_pid() -> u32 {
    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/C", "exit", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn no-op cmd child");
    #[cfg(not(windows))]
    let mut child = Command::new("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn no-op true child");

    let pid = child.id();
    let status = child.wait().expect("wait no-op child");
    assert!(status.success());
    pid
}

fn splitmix64_bytes(mut state: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(len);
    out
}

// =============================================================================
// Feature F1: Cargo Workspace & Single Static Binary (5 Tier-2 Tests)
// =============================================================================

#[test]
fn tier2_f01_unknown_flag_exits_nonzero() {
    let repo = TestRepo::new().unwrap();
    let out_global = repo.loom(&["--nonexistent-flag-xyz"]);
    assert_clean_failure(&out_global);

    let out_sub = repo.loom(&["status", "--nonexistent-flag-xyz"]);
    assert_clean_failure(&out_sub);

    assert!(!repo.exists(".loom"));
}

#[test]
fn tier2_f01_unknown_subcommand_exits_nonzero() {
    let repo = TestRepo::new().unwrap();
    let out = repo.loom(&["nonexistent-subcommand-xyz"]);
    assert_clean_failure(&out);
    assert!(!repo.exists(".loom"));
}

#[test]
fn tier2_f01_command_outside_initialized_repo() {
    let repo = TestRepo::new().unwrap();
    for args in [&["status"][..], &["list"][..], &["log"][..]] {
        let out = repo.loom(args);
        assert_clean_failure(&out);
    }
    assert!(!repo.exists(".loom"));
    assert_eq!(repo.daemon_pid(), None);
}

#[test]
fn tier2_f01_hidden_daemon_invalid_repo_path() {
    let repo = TestRepo::new().unwrap();

    // 1. Missing required --repo flag
    let bg_missing = repo
        .spawn_loom_bg(&["__daemon"], None, &[])
        .expect("spawn __daemon without --repo");
    let status_missing = bg_missing
        .wait_timeout(Duration::from_secs(5))
        .expect("__daemon without --repo must exit promptly");
    assert!(!status_missing.success());
    assert!(status_missing.code().is_some());
    assert_ne!(status_missing.code(), Some(STATUS_DLL_NOT_FOUND_EXIT));
    assert!(!bg_missing.stderr_str().contains("panicked at"));

    // 2. Existing regular file passed as --repo
    repo.write_file("regular_file.txt", "not a directory\n")
        .unwrap();
    let file_path = repo
        .path("regular_file.txt")
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let bg_file = repo
        .spawn_loom_bg(&["__daemon", "--repo", &file_path], None, &[])
        .expect("spawn __daemon with regular file --repo");
    let status_file = bg_file
        .wait_timeout(Duration::from_secs(5))
        .expect("__daemon with file --repo must exit promptly");
    assert!(!status_file.success());
    assert!(status_file.code().is_some());
    assert_ne!(status_file.code(), Some(STATUS_DLL_NOT_FOUND_EXIT));
    let combined = format!("{}\n{}", bg_file.stdout_str(), bg_file.stderr_str());
    assert!(!combined.trim().is_empty());
    assert!(!combined.contains("panicked at"));
    assert!(!combined.contains("RUST_BACKTRACE"));
    assert!(!repo.exists(".loom"));
}

#[test]
fn tier2_f01_empty_args_prints_usage() {
    let repo = TestRepo::new().unwrap();
    let out = repo.loom(&[]);
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());

    let combined = format!("{}\n{}", out.stdout_str(), out.stderr_str());
    assert!(!combined.trim().is_empty(), "expected usage output");
    let lower = combined.to_ascii_lowercase();
    assert!(
        lower.contains("usage") || lower.contains("loom") || lower.contains("init"),
        "expected CLI usage keywords in:\n{combined}"
    );
    assert!(!repo.exists(".loom"));
}

// =============================================================================
// Feature F2: .loom/ Layout & Config (5 Tier-2 Tests)
// =============================================================================

#[test]
fn tier2_f02_reinit_already_initialized_repo() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn keep() -> i32 { 7 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    let out = repo.loom(&["init"]);
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());
    if !out.success() {
        assert_clean_failure(&out);
    }

    assert!(repo.exists(".loom/config.toml"));
    assert!(repo.exists(".loom/state.redb"));
    assert_eq!(
        repo.read_file("src/lib.rs").unwrap(),
        "pub fn keep() -> i32 { 7 }\n"
    );
    repo.loom(&["status"]).assert_success();
    repo.loom(&["log"]).assert_success();
}

#[test]
fn tier2_f02_corrupt_config_toml_clean_error() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    kill_daemon(&repo);

    let cfg = repo.path(".loom/config.toml").unwrap();
    write_bytes_retry(&cfg, b"idle_timeout_secs = [unterminated\n[[[broken");

    let out = repo.loom(&["status"]);
    assert_clean_failure(&out);
}

#[test]
fn tier2_f02_zero_idle_timeout_config_boundary() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    kill_daemon(&repo);

    let cfg = repo.path(".loom/config.toml").unwrap();
    set_idle_timeout_in_config(&cfg, 0);

    let start = Instant::now();
    let out = repo.loom(&["status"]);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "zero idle timeout caused hang"
    );
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());
    if !out.success() {
        assert!(
            !out.stderr_str().trim().is_empty() || !out.stdout_str().trim().is_empty(),
            "non-zero exit should report a clean diagnostic"
        );
    }
}

#[test]
fn tier2_f02_missing_config_toml_recovery_or_error() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    kill_daemon(&repo);

    remove_file_retry(&repo.path(".loom/config.toml").unwrap());
    assert!(!repo.exists(".loom/config.toml"));

    let out = repo.loom(&["status"]);
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());
    if !out.success() {
        assert_clean_failure(&out);
    }
    assert!(repo.exists(".loom/state.redb"));
}

#[test]
fn tier2_f02_init_colocate_without_git_dir() {
    let repo = TestRepo::new().unwrap();
    assert!(!repo.exists(".git"));

    let out = repo.loom(&["init", "--colocate-git"]);
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());
    if out.success() {
        let config_raw = repo
            .read_file(".loom/config.toml")
            .expect("read .loom/config.toml after init --colocate-git");
        let config_val: toml::Value =
            toml::from_str(&config_raw).expect("parse .loom/config.toml");
        assert!(
            repo.exists(".git") || has_colocate_git_enabled(&config_val),
            "init --colocate-git succeeded without creating .git or setting colocate_git = true in config.toml"
        );
        repo.loom(&["status"]).assert_success();
    } else {
        assert_clean_failure(&out);
    }
}

// =============================================================================
// Feature F3: state.redb 8-Table Schema (5 Tier-2 Tests)
// =============================================================================

#[test]
fn tier2_f03_corrupted_state_redb_clean_error() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    assert!(repo.exists(".loom/state.redb"));
    kill_daemon(&repo);

    write_bytes_retry(&repo.path(".loom/state.redb").unwrap(), &vec![0xDEu8; 4096]);

    assert_clean_failure(&repo.loom(&["status"]));
    assert_clean_failure(&repo.loom(&["list"]));
}

#[test]
fn tier2_f03_zero_byte_state_redb() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    kill_daemon(&repo);

    write_bytes_retry(&repo.path(".loom/state.redb").unwrap(), &[]);

    let out = repo.loom(&["status"]);
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());
    if out.success() {
        repo.loom(&["log"]).assert_success();
    } else {
        assert_clean_failure(&out);
    }
}

#[test]
fn tier2_f03_empty_symbol_query_in_db() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let out = repo.loom(&["log", "--symbol", ""]);
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());
    if !out.success() {
        assert_clean_failure(&out);
    }
    repo.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f03_unicode_and_long_fqn_keys() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let out_unicode = repo.loom(&["log", "--symbol", "모듈::한글_함수::🦀_calc"]);
    out_unicode.assert_success();
    assert_no_crash_signatures(&out_unicode);

    let out_unicode_json = repo.loom(&["log", "--symbol", "모듈::한글_함수::🦀_calc", "--json"]);
    out_unicode_json.assert_success();
    assert_no_crash_signatures(&out_unicode_json);
    let _: Value = out_unicode_json
        .stdout_json()
        .expect("valid JSON from unicode --symbol query");

    let long_fqn = format!("{}leaf_fn", "ns_segment::".repeat(180));
    assert!(long_fqn.len() > 2048);
    let out_long = repo.loom(&["log", "--symbol", &long_fqn]);
    assert_no_crash_signatures(&out_long);
    assert!(out_long.exit_code().is_some());
    if out_long.success() {
        let out_long_json = repo.loom(&["log", "--symbol", &long_fqn, "--json"]);
        out_long_json.assert_success();
        assert_no_crash_signatures(&out_long_json);
        let _: Value = out_long_json
            .stdout_json()
            .expect("valid JSON from long FQN --symbol query");
    } else {
        assert_clean_failure(&out_long);
    }

    repo.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f03_high_op_log_sequence_boundary() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    for _ in 0..10 {
        let gc_out = repo.loom(&["gc"]);
        gc_out.assert_success();
        assert_no_crash_signatures(&gc_out);
    }

    kill_daemon(&repo);
    let log_out = repo.loom(&["log", "--json"]);
    log_out.assert_success();
    assert_no_crash_signatures(&log_out);
    let _parsed: Value = log_out.stdout_json().expect("valid JSON from log --json");

    let u64_max_str = u64::MAX.to_string();
    let out_u64_max = repo.loom(&["undo", "--op", &u64_max_str]);
    assert_clean_failure(&out_u64_max);

    let i64_max_str = i64::MAX.to_string();
    let out_i64_max = repo.loom(&["undo", "--op", &i64_max_str]);
    assert_clean_failure(&out_i64_max);

    repo.loom(&["status"]).assert_success();
}

// =============================================================================
// Feature F4: CAS Store (BLAKE3 + Zstd-3) (5 Tier-2 Tests)
// =============================================================================

#[test]
fn tier2_f04_zero_byte_file_cas_roundtrip() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp0", "-m", "v1 zero"])
        .assert_success();
    let vp0 = repo.viewport_dir("vp0");
    write_bytes_retry(&vp0.join("empty.bin"), &[]);
    repo.loom_in(&vp0, &["seal", "-m", "v1 zero"])
        .assert_success();
    assert_eq!(repo.read_bytes("empty.bin").unwrap(), Vec::<u8>::new());

    repo.loom(&["spawn", "-n", "vp1", "-m", "v2 non-empty"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    write_bytes_retry(&vp1.join("empty.bin"), b"non-empty");
    repo.loom_in(&vp1, &["seal", "-m", "v2 non-empty"])
        .assert_success();
    assert_eq!(repo.read_bytes("empty.bin").unwrap(), b"non-empty");

    let undo_out = repo.loom(&["undo"]);
    undo_out.assert_success();
    assert_no_crash_signatures(&undo_out);
    assert_eq!(repo.read_bytes("empty.bin").unwrap(), Vec::<u8>::new());

    let blobs = find_cas_blobs(repo.root());
    assert!(!blobs.is_empty(), "expected CAS blobs to be materialized");
    assert!(blobs.iter().any(|p| {
        fs::read(p)
            .map(|b| b.len() >= 4 && b[..4] == [0x28, 0xB5, 0x2F, 0xFD])
            .unwrap_or(false)
    }));
}

#[test]
fn tier2_f04_corrupt_cas_blob_detected_by_blake3() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let payload_v1 = vec![0xAAu8; 4096];
    let payload_v2 = vec![0xBBu8; 4096];

    repo.loom(&["spawn", "-n", "vp0", "-m", "v1"])
        .assert_success();
    let vp0 = repo.viewport_dir("vp0");
    write_bytes_retry(&vp0.join("data.bin"), &payload_v1);
    repo.loom_in(&vp0, &["seal", "-m", "v1"]).assert_success();

    repo.loom(&["spawn", "-n", "vp1", "-m", "v2"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    write_bytes_retry(&vp1.join("data.bin"), &payload_v2);
    repo.loom_in(&vp1, &["seal", "-m", "v2"]).assert_success();

    kill_daemon(&repo);
    let blobs = find_cas_blobs(repo.root());
    assert!(!blobs.is_empty(), "expected CAS blobs after two seals");

    for blob in &blobs {
        let mut bytes = fs::read(blob).expect("read CAS blob");
        assert!(!bytes.is_empty());
        let mid = bytes.len() / 2;
        if bytes.len() > 4 {
            let idx = mid.max(4);
            bytes[idx] ^= 0xFF;
            let last = bytes.len() - 1;
            if last > 4 && last != idx {
                bytes[last] ^= 0xFF;
            }
        } else {
            bytes[0] ^= 0xFF;
        }
        write_bytes_retry(blob, &bytes);
    }

    let undo_out = repo.loom(&["undo"]);
    assert_clean_failure(&undo_out);
    assert_eq!(repo.read_bytes("data.bin").unwrap(), payload_v2);
}

#[test]
fn tier2_f04_missing_cas_blob_clean_error() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let payload_v1 = vec![0xAAu8; 4096];
    let payload_v2 = vec![0xBBu8; 4096];

    repo.loom(&["spawn", "-n", "vp0", "-m", "v1"])
        .assert_success();
    let vp0 = repo.viewport_dir("vp0");
    write_bytes_retry(&vp0.join("data.bin"), &payload_v1);
    repo.loom_in(&vp0, &["seal", "-m", "v1"]).assert_success();

    repo.loom(&["spawn", "-n", "vp1", "-m", "v2"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    write_bytes_retry(&vp1.join("data.bin"), &payload_v2);
    repo.loom_in(&vp1, &["seal", "-m", "v2"]).assert_success();

    kill_daemon(&repo);
    let blobs = find_cas_blobs(repo.root());
    assert!(!blobs.is_empty(), "expected CAS blobs after two seals");
    for blob in &blobs {
        remove_file_retry(blob);
    }

    let undo_out = repo.loom(&["undo"]);
    assert_clean_failure(&undo_out);
    assert_eq!(repo.read_bytes("data.bin").unwrap(), payload_v2);
}

#[test]
fn tier2_f04_incompressible_random_bytes_cas() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let random_bytes = splitmix64_bytes(0x9E37_79B9_7F4A_7C15, 262_144);

    repo.loom(&["spawn", "-n", "vp0", "-m", "random v1"])
        .assert_success();
    let vp0 = repo.viewport_dir("vp0");
    write_bytes_retry(&vp0.join("rand.bin"), &random_bytes);
    repo.loom_in(&vp0, &["seal", "-m", "random v1"])
        .assert_success();
    assert_eq!(repo.read_bytes("rand.bin").unwrap(), random_bytes);

    repo.loom(&["spawn", "-n", "vp1", "-m", "short v2"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    write_bytes_retry(&vp1.join("rand.bin"), b"short");
    repo.loom_in(&vp1, &["seal", "-m", "short v2"])
        .assert_success();

    let undo_out = repo.loom(&["undo"]);
    undo_out.assert_success();
    assert_no_crash_signatures(&undo_out);
    assert_eq!(repo.read_bytes("rand.bin").unwrap(), random_bytes);
    assert!(!find_cas_blobs(repo.root()).is_empty());
}

#[test]
fn tier2_f04_single_byte_file_cas() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp0", "-m", "one byte v1"])
        .assert_success();
    let vp0 = repo.viewport_dir("vp0");
    write_bytes_retry(&vp0.join("one.bin"), &[0x42]);
    repo.loom_in(&vp0, &["seal", "-m", "one byte v1"])
        .assert_success();
    assert_eq!(repo.read_bytes("one.bin").unwrap(), vec![0x42]);

    repo.loom(&["spawn", "-n", "vp1", "-m", "one byte v2"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    write_bytes_retry(&vp1.join("one.bin"), &[0xFF]);
    repo.loom_in(&vp1, &["seal", "-m", "one byte v2"])
        .assert_success();
    assert_eq!(repo.read_bytes("one.bin").unwrap(), vec![0xFF]);

    let undo_out = repo.loom(&["undo"]);
    undo_out.assert_success();
    assert_no_crash_signatures(&undo_out);
    assert_eq!(repo.read_bytes("one.bin").unwrap(), vec![0x42]);
}

// =============================================================================
// Feature F5: WAL & Crash Recovery (5 Tier-2 Tests)
// =============================================================================

#[test]
fn tier2_f05_torn_wal_tail_truncated_on_recovery() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["status"]).assert_success();
    kill_daemon(&repo);

    let segments = find_wal_segments(repo.root());
    assert!(!segments.is_empty(), "expected LMWA WAL segment after init");
    let latest_seg = segments.last().unwrap();
    let mut bytes = fs::read(latest_seg).expect("read WAL segment");
    let orig_len = bytes.len();
    assert!(orig_len >= 60, "expected full WAL frame, got {orig_len}B");

    bytes.extend_from_slice(b"LMWA\x01\x00\x01\x00\x99\x00\x00\x00torn");
    write_bytes_retry(latest_seg, &bytes);

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_crash_signatures(&status);
    repo.loom(&["list"]).assert_success();

    let log_json = repo.loom(&["log", "--json"]);
    log_json.assert_success();
    assert_no_crash_signatures(&log_json);
    let parsed: Value = log_json
        .stdout_json()
        .expect("valid JSON from log --json after torn WAL recovery");
    let has_entries = match &parsed {
        Value::Array(arr) => !arr.is_empty(),
        Value::Object(map) => !map.is_empty(),
        _ => false,
    };
    assert!(
        has_entries,
        "expected non-empty operation history after torn WAL recovery, got: {parsed}"
    );
    assert!(
        parsed.to_string().contains("RepoInit"),
        "expected RepoInit history to be preserved after torn WAL recovery, got: {parsed}"
    );

    let recovered_len = fs::metadata(latest_seg).expect("stat WAL segment").len();
    assert!(
        recovered_len >= 60 && recovered_len <= orig_len as u64,
        "expected torn tail truncated to [60, {orig_len}], got {recovered_len}"
    );
}

#[test]
fn tier2_f05_zero_byte_wal_tail_recovered() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["status"]).assert_success();
    kill_daemon(&repo);

    let segments = find_wal_segments(repo.root());
    assert!(!segments.is_empty(), "expected LMWA WAL segment after init");
    let latest_seg = segments.last().unwrap();
    let ext = latest_seg
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("log");
    let zero_tail_seg = latest_seg
        .parent()
        .unwrap()
        .join(format!("wal-9999999999999999.{ext}"));
    write_bytes_retry(&zero_tail_seg, &[]);

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_crash_signatures(&status);
    repo.loom(&["list"]).assert_success();
    repo.loom(&["log"]).assert_success();
}

#[test]
fn tier2_f05_midstream_wal_corruption_fails_cleanly() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    let (seg_path, _, frame1_off) = ensure_multi_frame_wal_segment(&repo);
    kill_daemon(&repo);

    let mut bytes = fs::read(&seg_path).expect("read multi-frame WAL segment");
    let frame1_off = find_second_wal_frame_offset(&bytes).unwrap_or(frame1_off);

    // Flip last byte of frame 0 (inside BLAKE3 MAC), leaving frame 1+ intact
    bytes[frame1_off - 1] ^= 0xFF;
    write_bytes_retry(&seg_path, &bytes);

    let out = repo.loom(&["status"]);
    assert_clean_failure(&out);
}

#[test]
fn tier2_f05_bad_magic_in_wal_segment() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    let (seg_path, _, _) = ensure_multi_frame_wal_segment(&repo);
    kill_daemon(&repo);

    let mut bytes = fs::read(&seg_path).expect("read WAL segment");
    assert!(bytes.len() >= 4);
    bytes[..4].copy_from_slice(b"BAD!");
    write_bytes_retry(&seg_path, &bytes);

    let out = repo.loom(&["status"]);
    assert_clean_failure(&out);
}

#[test]
fn tier2_f05_wal_replay_under_rapid_kill_cycle() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    for cycle in 0..5usize {
        let vp = format!("cycle-{cycle:02}");
        repo.loom(&["spawn", "-n", &vp, "-m", "rapid kill replay"])
            .assert_success();
        let killed_pid = kill_daemon(&repo).expect("daemon PID must exist");

        let list = repo.loom(&["list"]);
        list.assert_success();
        assert_no_crash_signatures(&list);
        let stdout = list.stdout_str();
        for prev in 0..=cycle {
            assert!(
                stdout.contains(&format!("cycle-{prev:02}")),
                "missing cycle-{prev:02} after rapid kill cycle {cycle}"
            );
        }
        if let Some(new_pid) = repo.daemon_pid() {
            assert_ne!(new_pid, killed_pid, "expected fresh daemon PID after kill");
        }
    }

    repo.loom(&["status"]).assert_success();
    repo.loom(&["log"]).assert_success();
}

// =============================================================================
// Feature F6: Single-Writer loomd & Idle Shutdown (5 Tier-2 Tests)
// =============================================================================

#[test]
fn tier2_f06_stale_daemon_sock_and_lock_recovery() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    kill_daemon(&repo);

    let dead_pid = spawn_and_wait_dead_pid();
    let transport = if cfg!(windows) {
        "named_pipe"
    } else {
        "unix_socket"
    };
    let endpoint = if cfg!(windows) {
        r"\\.\pipe\loom-stale-e2e-dead"
    } else {
        "/tmp/loom-stale-e2e-dead.sock"
    };
    let stale_sock = serde_json::json!({
        "transport": transport,
        "endpoint": endpoint,
        "pid": dead_pid,
        "protocol_version": 1
    });

    write_bytes_retry(
        &repo.path(".loom/daemon.sock").unwrap(),
        serde_json::to_vec(&stale_sock).unwrap().as_slice(),
    );
    write_bytes_retry(&repo.path(".loom/daemon.lock").unwrap(), b"stale-lock");

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_crash_signatures(&status);

    let live_pid = repo
        .daemon_pid()
        .expect("daemon_pid must be populated after recovery");
    assert_ne!(
        live_pid, dead_pid,
        "expected new live daemon PID replacing dead PID {dead_pid}"
    );
}

#[test]
fn tier2_f06_corrupt_daemon_sock_descriptor() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    kill_daemon(&repo);

    write_bytes_retry(
        &repo.path(".loom/daemon.sock").unwrap(),
        b"{corrupt_sock_descriptor",
    );

    let out = repo.loom(&["status"]);
    assert_no_crash_signatures(&out);
    assert!(out.exit_code().is_some());
    if out.success() {
        let sock_text = repo.read_file(".loom/daemon.sock").unwrap();
        let parsed: Value =
            serde_json::from_str(&sock_text).expect("daemon.sock should recover to valid JSON");
        assert!(
            parsed.get("pid").and_then(Value::as_u64).unwrap_or(0) > 0,
            "recovered daemon.sock must include live pid"
        );
    } else {
        assert_clean_failure(&out);
    }
}

#[test]
fn tier2_f06_parallel_spawn_race_same_viewport() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let threads = 6usize;
    let barrier = Barrier::new(threads);
    let outputs: Vec<CommandOutput> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    barrier.wait();
                    repo.try_loom_in_with_env(
                        repo.root(),
                        &["spawn", "-n", "race-vp", "-m", "race"],
                        &[],
                    )
                    .expect("spawn loom process")
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for out in &outputs {
        assert_no_crash_signatures(out);
        assert!(out.exit_code().is_some());
    }

    let wins = outputs.iter().filter(|o| o.success()).count();
    assert_eq!(wins, 1, "expected exactly 1 winner in spawn race, got {wins}");
    for loser in outputs.iter().filter(|o| !o.success()) {
        assert_clean_failure(loser);
    }

    let list = repo.loom(&["list"]);
    list.assert_success();
    assert!(list.stdout_str().contains("race-vp"));
    assert!(repo.viewport_dir("race-vp").is_dir());
}

#[test]
fn tier2_f06_negative_idle_timeout_rejected() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    kill_daemon(&repo);

    let cfg = repo.path(".loom/config.toml").unwrap();
    set_idle_timeout_in_config(&cfg, -5);

    let out = repo.loom(&["status"]);
    assert_clean_failure(&out);
}

#[test]
fn tier2_f06_daemon_handles_16_concurrent_clients() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let n = 16usize;
    let barrier = Barrier::new(n);
    let outputs: Vec<(usize, CommandOutput)> = std::thread::scope(|s| {
        let repo_ref = &repo;
        let barrier_ref = &barrier;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                s.spawn(move || {
                    barrier_ref.wait();
                    let args: &[&str] = match i % 3 {
                        0 => &["status", "--json"],
                        1 => &["list", "--json"],
                        _ => &["log", "--json"],
                    };
                    let out = repo_ref
                        .try_loom_in_with_env(repo_ref.root(), args, &[])
                        .expect("spawn concurrent loom client");
                    (i, out)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut observed_pids = Vec::new();
    for (i, out) in &outputs {
        out.assert_success();
        assert_no_crash_signatures(out);
        let json = out
            .stdout_json()
            .unwrap_or_else(|e| panic!("client {i} returned invalid JSON: {e}"));
        if i % 3 == 0 {
            if let Some(pid) = json.get("pid").and_then(Value::as_u64) {
                observed_pids.push(pid);
            }
        }
    }

    let daemon_pid = repo.daemon_pid().expect("live daemon pid");
    assert!(!observed_pids.is_empty());
    for pid in observed_pids {
        assert_eq!(
            pid,
            u64::from(daemon_pid),
            "all concurrent clients must talk to the same single daemon"
        );
    }
}

// =============================================================================
// Feature F7: R1 CLI Commands (5 Tier-2 Tests)
// =============================================================================

#[test]
fn tier2_f07_undo_empty_history_clean_error() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let out = repo.loom(&["undo"]);
    assert_clean_failure(&out);

    repo.loom(&["status"]).assert_success();
    repo.loom(&["log"]).assert_success();
}

#[test]
fn tier2_f07_undo_nonexistent_op_id() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["gc"]).assert_success();

    let out = repo.loom(&["undo", "--op", "9999999"]);
    assert_clean_failure(&out);

    repo.loom(&["status"]).assert_success();
    repo.loom(&["log"]).assert_success();
}

#[test]
fn tier2_f07_undo_zero_or_negative_op_id() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let out_zero = repo.loom(&["undo", "--op", "0"]);
    assert_clean_failure(&out_zero);

    let out_neg = repo.loom(&["undo", "--op", "-1"]);
    assert_clean_failure(&out_neg);

    repo.loom(&["status"]).assert_success();
    repo.loom(&["log"]).assert_success();
}

#[test]
fn tier2_f07_log_nonexistent_symbol() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let out_human = repo.loom(&["log", "--symbol", "nonexistent::module::NeverDefinedSymbol999"]);
    out_human.assert_success();
    assert_no_crash_signatures(&out_human);
    assert!(
        !out_human.stdout_str().contains("NeverDefinedSymbol999"),
        "log output should not fabricate nonexistent symbol"
    );

    let out_json = repo.loom(&["log", "--capsules", "--symbol", "nonexistent::Symbol", "--json"]);
    out_json.assert_success();
    assert_no_crash_signatures(&out_json);
    let val = out_json.stdout_json().expect("valid JSON output");
    if let Some(arr) = val.as_array() {
        assert!(arr.is_empty(), "expected empty array for nonexistent symbol");
    } else {
        assert!(
            !val.to_string().contains("nonexistent::Symbol"),
            "unexpected symbol match in JSON: {val}"
        );
    }
}

#[test]
fn tier2_f07_gc_on_active_viewport_does_not_prune_active() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn a() {}\n").unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-active", "-m", "keep active"])
        .assert_success();
    let vp_active = repo.viewport_dir("vp-active");
    write_bytes_retry(
        &vp_active.join("src").join("lib.rs"),
        b"pub fn a() {}\npub fn wip() {}\n",
    );

    repo.loom(&["spawn", "-n", "vp-done", "-m", "seal me"])
        .assert_success();
    let vp_done = repo.viewport_dir("vp-done");
    write_bytes_retry(
        &vp_done.join("src").join("done.rs"),
        b"pub fn done() -> i32 { 1 }\n",
    );
    repo.loom_in(&vp_done, &["seal", "-m", "done"])
        .assert_success();

    let gc_prune = repo.loom(&["gc", "--prune-viewports"]);
    gc_prune.assert_success();
    assert_no_crash_signatures(&gc_prune);

    let gc_plain = repo.loom(&["gc"]);
    gc_plain.assert_success();
    assert_no_crash_signatures(&gc_plain);

    assert!(
        vp_active.is_dir(),
        "gc --prune-viewports must not prune active viewport"
    );
    let active_content = fs::read_to_string(vp_active.join("src").join("lib.rs"))
        .expect("read active viewport source");
    assert!(active_content.contains("pub fn wip() {}"));

    let list = repo.loom(&["list"]);
    list.assert_success();
    assert!(list.stdout_str().contains("vp-active"));
}
