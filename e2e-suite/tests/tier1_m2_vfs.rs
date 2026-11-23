use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use e2e_suite::TestRepo;

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

#[cfg(windows)]
fn open_exclusive_rw(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .open(path)
}

#[cfg(not(windows))]
fn open_exclusive_rw(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
}

// ============================================================================
// Feature F8: Cross-Platform ViewportDriver (5 tests)
// ============================================================================

#[test]
fn tier1_f08_ntfs_fallback_driver_selected() {
    let repo = TestRepo::new().unwrap();
    let orig = "pub fn ok() -> i32 { 1 }\n";
    repo.write_file("src/lib.rs", orig).unwrap();
    repo.loom(&["init"]).assert_success();

    let spawn_out = repo.loom(&["spawn", "-n", "vp-ntfs", "-m", "driver check"]);
    spawn_out.assert_success();
    assert_eq!(spawn_out.exit_code(), Some(0));

    let status_out = repo.loom(&["status"]);
    status_out.assert_success();
    assert_eq!(status_out.exit_code(), Some(0));

    let vp = repo.viewport_dir("vp-ntfs");
    assert!(vp.is_dir(), "viewport directory should exist");
    assert_eq!(
        fs::read_to_string(vp.join("src/lib.rs")).unwrap(),
        orig,
        "viewport file should match trunk"
    );

    let combined = format!(
        "{}\n{}",
        normalize_newlines(&spawn_out.stdout_str()),
        normalize_newlines(&status_out.stdout_str())
    )
    .to_lowercase();
    if cfg!(windows) && (combined.contains("driver") || combined.contains("backend")) {
        assert!(
            combined.contains("ntfs")
                || combined.contains("windowsntfsfallback")
                || combined.contains("refs")
                || combined.contains("projfs"),
            "unexpected Windows driver output: {combined}"
        );
    }
}

#[test]
fn tier1_f08_editable_source_isolated_from_trunk() {
    let repo = TestRepo::new().unwrap();
    let orig = "pub fn trunk_val() -> i32 { 1 }\n";
    repo.write_file("src/lib.rs", orig).unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-iso", "-m", "iso"])
        .assert_success();

    let vp = repo.viewport_dir("vp-iso");
    fs::write(vp.join("src/lib.rs"), "pub fn trunk_val() -> i32 { 777 }\n").unwrap();
    fs::write(vp.join("src/new_only.rs"), "pub fn vp_only() {}\n").unwrap();

    assert_eq!(repo.read_file("src/lib.rs").unwrap(), orig);
    assert!(!repo.exists("src/new_only.rs"));
    assert!(fs::read_to_string(vp.join("src/lib.rs"))
        .unwrap()
        .contains("777"));
    assert!(vp.join("src/new_only.rs").is_file());
}

#[test]
fn tier1_f08_editable_source_isolated_between_viewports() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/shared.rs", "pub const V: i32 = 0;\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "e1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "e2"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    fs::write(vp1.join("src/shared.rs"), "pub const V: i32 = 1;\n").unwrap();
    fs::write(vp2.join("src/shared.rs"), "pub const V: i32 = 2;\n").unwrap();

    assert!(fs::read_to_string(vp1.join("src/shared.rs"))
        .unwrap()
        .contains("1"));
    assert!(fs::read_to_string(vp2.join("src/shared.rs"))
        .unwrap()
        .contains("2"));
    assert!(repo.read_file("src/shared.rs").unwrap().contains("0"));
}

#[test]
fn tier1_f08_restore_clean_reverts_viewport_edits() {
    let repo = TestRepo::new().unwrap();
    let orig = "pub fn clean() -> i32 { 1 }\n";
    repo.write_file("src/lib.rs", orig).unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-clean", "-m", "dirty"])
        .assert_success();

    let vp = repo.viewport_dir("vp-clean");
    fs::write(vp.join("src/lib.rs"), "pub fn clean() -> i32 { 999 }\n").unwrap();
    fs::write(vp.join("src/dirty_extra.rs"), "pub fn extra() {}\n").unwrap();

    repo.loom_in(&vp, &["drop", "--reason", "revert dirty edits"])
        .assert_success();

    assert_eq!(repo.read_file("src/lib.rs").unwrap(), orig);
    assert!(!repo.exists("src/dirty_extra.rs"));
    if vp.exists() {
        assert_eq!(fs::read_to_string(vp.join("src/lib.rs")).unwrap(), orig);
        assert!(!vp.join("src/dirty_extra.rs").exists());
    }
}

#[test]
fn tier1_f08_teardown_removes_viewport_directory() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn t() {}\n").unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-td", "-m", "teardown"])
        .assert_success();

    let vp = repo.viewport_dir("vp-td");
    assert!(vp.is_dir());

    repo.loom_in(&vp, &["drop", "--reason", "done"])
        .assert_success();
    repo.loom(&["gc", "--prune-viewports"]).assert_success();

    assert!(
        !vp.exists(),
        "viewport directory should be removed after gc --prune-viewports"
    );
    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let list_lower = normalize_newlines(&list_out.stdout_str()).to_lowercase();
    for line in list_lower.lines() {
        if line.contains("vp-td") {
            assert!(
                !line.contains("active"),
                "pruned viewport should not be listed as active: {line}"
            );
        }
    }
}

// ============================================================================
// Feature F9: Dependency Block Sharing & Lock Isolation (5 tests)
// ============================================================================

#[test]
fn tier1_f09_node_modules_materialized_fast_and_present() {
    let repo = TestRepo::new().unwrap();
    for i in 0..30 {
        let rel = format!("node_modules/pkg-{i}/index.js");
        let body = format!("module.exports = {{ id: {i} }};\n");
        repo.write_file(rel, &body).unwrap();
    }
    let orig_ts = "export const val = 1;\n";
    repo.write_file("src/index.ts", orig_ts).unwrap();
    repo.loom(&["init"]).assert_success();

    let start = Instant::now();
    repo.loom(&["spawn", "-n", "vp-node", "-m", "node test"])
        .assert_success();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "spawning viewport with node_modules took too long: {elapsed:?}"
    );

    let vp = repo.viewport_dir("vp-node");
    for i in 0..30 {
        let pkg_file = vp.join(format!("node_modules/pkg-{i}/index.js"));
        let expected = format!("module.exports = {{ id: {i} }};\n");
        assert_eq!(fs::read_to_string(&pkg_file).unwrap(), expected);
    }

    fs::write(vp.join("src/index.ts"), "export const val = 2;\n").unwrap();
    assert_eq!(repo.read_file("src/index.ts").unwrap(), orig_ts);
}

#[test]
fn tier1_f09_venv_cache_accessible_per_viewport() {
    let repo = TestRepo::new().unwrap();
    let pyvenv = "home = C:\\Python312\n";
    let mod_a = "X = 42\n";
    repo.write_file(".venv/pyvenv.cfg", pyvenv).unwrap();
    repo.write_file(".venv/Lib/site-packages/mod_a.py", mod_a)
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-py1", "-m", "py1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp-py2", "-m", "py2"])
        .assert_success();

    for name in ["vp-py1", "vp-py2"] {
        let vp = repo.viewport_dir(name);
        assert_eq!(
            fs::read_to_string(vp.join(".venv/pyvenv.cfg")).unwrap(),
            pyvenv
        );
        assert_eq!(
            fs::read_to_string(vp.join(".venv/Lib/site-packages/mod_a.py")).unwrap(),
            mod_a
        );
    }
}

#[test]
fn tier1_f09_cargo_lock_isolated_per_viewport() {
    let repo = TestRepo::new().unwrap();
    repo.write_bytes("target/debug/.cargo-lock", b"trunk-lock")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-l1", "-m", "l1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp-l2", "-m", "l2"])
        .assert_success();

    let vp1_lock = repo.viewport_dir("vp-l1").join("target/debug/.cargo-lock");
    let vp2_lock = repo.viewport_dir("vp-l2").join("target/debug/.cargo-lock");
    fs::create_dir_all(vp1_lock.parent().unwrap()).unwrap();
    fs::create_dir_all(vp2_lock.parent().unwrap()).unwrap();

    {
        let mut guard1 = open_exclusive_rw(&vp1_lock).expect("exclusive open vp-l1 .cargo-lock");
        let mut guard2 = open_exclusive_rw(&vp2_lock).expect("exclusive open vp-l2 .cargo-lock");
        guard1.set_len(0).unwrap();
        guard1.write_all(b"vp1-lock").unwrap();
        guard1.flush().unwrap();

        guard2.set_len(0).unwrap();
        guard2.write_all(b"vp2-lock").unwrap();
        guard2.flush().unwrap();
    }

    assert_eq!(fs::read(&vp1_lock).unwrap(), b"vp1-lock");
    assert_eq!(fs::read(&vp2_lock).unwrap(), b"vp2-lock");
    assert_eq!(
        repo.read_bytes("target/debug/.cargo-lock").unwrap(),
        b"trunk-lock"
    );
}

#[test]
fn tier1_f09_target_output_binary_isolated() {
    let repo = TestRepo::new().unwrap();
    repo.write_bytes("target/debug/app.exe", b"TRUNK_EXE")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-b1", "-m", "b1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp-b2", "-m", "b2"])
        .assert_success();

    let vp1_bin = repo.viewport_dir("vp-b1").join("target/debug/app.exe");
    let vp2_bin = repo.viewport_dir("vp-b2").join("target/debug/app.exe");
    fs::create_dir_all(vp1_bin.parent().unwrap()).unwrap();
    fs::create_dir_all(vp2_bin.parent().unwrap()).unwrap();

    {
        let mut guard1 = open_exclusive_rw(&vp1_bin).expect("exclusive open vp-b1 app.exe");
        let mut guard2 = open_exclusive_rw(&vp2_bin).expect("exclusive open vp-b2 app.exe");
        guard1.set_len(0).unwrap();
        guard1.write_all(b"VP1_EXE").unwrap();
        guard1.flush().unwrap();

        guard2.set_len(0).unwrap();
        guard2.write_all(b"VP2_EXE").unwrap();
        guard2.flush().unwrap();
    }

    assert_eq!(fs::read(&vp1_bin).unwrap(), b"VP1_EXE");
    assert_eq!(fs::read(&vp2_bin).unwrap(), b"VP2_EXE");
    assert_eq!(
        repo.read_bytes("target/debug/app.exe").unwrap(),
        b"TRUNK_EXE"
    );
}

#[test]
fn tier1_f09_immutable_rlib_cache_shared_safely() {
    let repo = TestRepo::new().unwrap();
    let rlib = vec![0x7Fu8; 8192];
    repo.write_bytes("target/debug/deps/libalpha.rlib", &rlib)
        .unwrap();
    let orig_src = "pub fn s() {}\n";
    repo.write_file("src/lib.rs", orig_src).unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-r1", "-m", "r1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp-r2", "-m", "r2"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp-r1");
    let vp2 = repo.viewport_dir("vp-r2");
    let p1 = vp1.join("target/debug/deps/libalpha.rlib");
    let p2 = vp2.join("target/debug/deps/libalpha.rlib");

    fs::write(vp1.join("src/lib.rs"), "pub fn s() { let _ = 1; }\n").unwrap();

    let (b1, b2) = std::thread::scope(|s| {
        let h1 = s.spawn(|| fs::read(&p1).unwrap());
        let h2 = s.spawn(|| fs::read(&p2).unwrap());
        (h1.join().unwrap(), h2.join().unwrap())
    });

    assert_eq!(b1, rlib);
    assert_eq!(b2, rlib);
    assert_eq!(fs::read_to_string(vp2.join("src/lib.rs")).unwrap(), orig_src);
    assert_eq!(repo.read_file("src/lib.rs").unwrap(), orig_src);
}

// ============================================================================
// Feature F10: Viewport Lifecycle & `loom spawn` (5 tests)
// ============================================================================

#[test]
fn tier1_f10_spawn_creates_active_viewport() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-active", "-m", "implement login"])
        .assert_success();

    assert!(repo.viewport_dir("vp-active").is_dir());

    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let list_lower = normalize_newlines(&list_out.stdout_str()).to_lowercase();
    assert!(list_lower.contains("vp-active"));
    assert!(list_lower.contains("active"));

    let status_out = repo.loom(&["status"]);
    status_out.assert_success();
    let status_lower = normalize_newlines(&status_out.stdout_str()).to_lowercase();
    assert!(status_lower.contains("vp-active"));
    assert!(status_lower.contains("active"));
}

#[test]
fn tier1_f10_spawn_runs_wrapped_command_in_viewport() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    #[cfg(windows)]
    let out = repo.loom(&[
        "spawn",
        "-n",
        "vp-cmd",
        "-m",
        "run cmd",
        "--",
        "cmd",
        "/C",
        "cd > cwd_marker.txt && echo wrapped_ok",
    ]);
    #[cfg(not(windows))]
    let out = repo.loom(&[
        "spawn",
        "-n",
        "vp-cmd",
        "-m",
        "run cmd",
        "--",
        "sh",
        "-c",
        "pwd > cwd_marker.txt && echo wrapped_ok",
    ]);
    out.assert_success();

    let vp = repo.viewport_dir("vp-cmd");
    let marker = vp.join("cwd_marker.txt");
    assert!(
        marker.is_file(),
        "wrapped command did not write cwd_marker.txt inside viewport"
    );

    let marker_raw = fs::read_to_string(&marker).unwrap();
    let marker_canon = fs::canonicalize(marker_raw.trim()).unwrap();
    let vp_canon = fs::canonicalize(&vp).unwrap();
    assert_eq!(marker_canon, vp_canon);
    assert!(!repo.exists("cwd_marker.txt"));
}

#[test]
fn tier1_f10_lifecycle_active_to_sealed() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn s() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-seal", "-m", "seal test"])
        .assert_success();

    let vp = repo.viewport_dir("vp-seal");
    fs::write(vp.join("src/lib.rs"), "pub fn s() -> i32 { 2 }\n").unwrap();
    repo.loom_in(&vp, &["seal", "-m", "done"]).assert_success();

    assert!(repo.read_file("src/lib.rs").unwrap().contains("2"));

    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let list_lower = normalize_newlines(&list_out.stdout_str()).to_lowercase();
    assert!(list_lower.contains("vp-seal"));
    assert!(list_lower.contains("sealed"));

    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    assert!(normalize_newlines(&log_out.stdout_str()).contains("done"));
}

#[test]
fn tier1_f10_lifecycle_active_to_dropped() {
    let repo = TestRepo::new().unwrap();
    let orig = "pub fn d() -> i32 { 1 }\n";
    repo.write_file("src/lib.rs", orig).unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-drop", "-m", "drop test"])
        .assert_success();

    let vp = repo.viewport_dir("vp-drop");
    fs::write(vp.join("src/lib.rs"), "pub fn d() -> i32 { 888 }\n").unwrap();
    repo.loom_in(&vp, &["drop", "--reason", "dead end"])
        .assert_success();

    assert_eq!(repo.read_file("src/lib.rs").unwrap(), orig);

    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let list_lower = normalize_newlines(&list_out.stdout_str()).to_lowercase();
    assert!(list_lower.contains("vp-drop"));
    assert!(list_lower.contains("dropped"));
}

#[test]
fn tier1_f10_lifecycle_sealed_to_gc_pending_and_pruned() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn g() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-g", "-m", "gc me"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp-stay", "-m", "keep me"])
        .assert_success();

    let vp_g = repo.viewport_dir("vp-g");
    fs::write(vp_g.join("src/lib.rs"), "pub fn g() -> i32 { 3 }\n").unwrap();
    repo.loom_in(&vp_g, &["seal", "-m", "ready for gc"])
        .assert_success();
    repo.loom(&["gc", "--prune-viewports"]).assert_success();

    assert!(!vp_g.exists(), "pruned viewport vp-g should be removed");
    assert!(
        repo.viewport_dir("vp-stay").is_dir(),
        "active viewport vp-stay must remain"
    );

    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let list_lower = normalize_newlines(&list_out.stdout_str()).to_lowercase();
    assert!(list_lower.contains("vp-stay"));
    assert!(list_lower.contains("active"));
    assert!(repo.read_file("src/lib.rs").unwrap().contains("3"));
}
