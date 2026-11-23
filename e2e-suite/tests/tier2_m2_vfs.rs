use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use e2e_suite::{CommandOutput, TestRepo};

fn assert_no_panic(out: &CommandOutput) {
    let stdout = out.stdout_str();
    let stderr = out.stderr_str();
    assert!(
        !stdout.contains("panicked at") && !stderr.contains("panicked at"),
        "command panicked (`panicked at` found)\nargs: {:?}\nstdout:\n{}\nstderr:\n{}",
        out.args,
        stdout,
        stderr
    );
    assert!(
        !stdout.contains("RUST_BACKTRACE") && !stderr.contains("RUST_BACKTRACE"),
        "command emitted RUST_BACKTRACE\nargs: {:?}\nstdout:\n{}\nstderr:\n{}",
        out.args,
        stdout,
        stderr
    );
}

#[track_caller]
fn assert_clean_failure(out: &CommandOutput) {
    out.assert_failure();
    assert_no_panic(out);
    assert!(
        !out.stderr_str().trim().is_empty() || !out.stdout_str().trim().is_empty(),
        "expected non-empty diagnostic on failure for args {:?}",
        out.args
    );
}

fn overwrite_in_place(path: &Path, content: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap_or_else(|e| panic!("failed to open {} for in-place overwrite: {e}", path.display()));
    file.write_all(content)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    file.flush()
        .unwrap_or_else(|e| panic!("failed to flush {}: {e}", path.display()));
}

fn open_exclusive_lock(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    opts.open(path)
}

fn set_readonly_flag(path: &Path, readonly: bool) {
    let meta = fs::metadata(path)
        .unwrap_or_else(|e| panic!("metadata failed for {}: {e}", path.display()));
    let mut perms = meta.permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms)
        .unwrap_or_else(|e| panic!("set_permissions({readonly}) failed for {}: {e}", path.display()));
}

struct ReadonlyGuard {
    paths: Vec<PathBuf>,
}

impl ReadonlyGuard {
    fn new() -> Self {
        Self { paths: Vec::new() }
    }

    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }
}

impl Drop for ReadonlyGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            if let Ok(meta) = fs::metadata(path) {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    perms.set_readonly(false);
                    let _ = fs::set_permissions(path, perms);
                }
            }
        }
    }
}

fn lines_for_viewport(output: &str, vp_name: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.contains(vp_name))
        .map(str::to_owned)
        .collect()
}

fn has_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == word)
}

// ============================================================================
// F8: Cross-Platform ViewportDriver (5 Tier 2 Boundary Tests)
// ============================================================================

#[test]
fn tier2_f08_empty_repo_spawn_materializes_cleanly() {
    let repo = TestRepo::new().expect("create TestRepo");
    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    let spawn = repo.loom(&["spawn", "-n", "vp-empty", "-m", "empty repo boundary"]);
    spawn.assert_success();
    assert_no_panic(&spawn);

    let vp = repo.viewport_dir("vp-empty");
    assert!(vp.is_dir(), "viewport directory missing: {}", vp.display());

    // Prevent recursive .loom/ internals inside viewport while tolerating RADAR_STATUS.json
    let vp_loom = vp.join(".loom");
    assert!(!vp_loom.join("viewports").exists());
    assert!(!vp_loom.join("state.redb").exists());
    assert!(!vp_loom.join("wal").exists());
    assert!(!vp_loom.join("cas").exists());

    // Zero user files materialized in empty repo (tolerate .gitignore iff `loom init` created it in trunk)
    let trunk_has_gitignore = repo.exists(".gitignore");
    for entry in fs::read_dir(&vp).expect("read viewport dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let allowed = name_str == ".loom" || (trunk_has_gitignore && name_str == ".gitignore");
        assert!(
            allowed,
            "unexpected non-.loom entry in empty viewport (trunk_has_gitignore={trunk_has_gitignore}): {:?}",
            name
        );
    }

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_panic(&status);
    assert!(
        status.stdout_str().contains("vp-empty"),
        "status output should mention vp-empty:\n{}",
        status.stdout_str()
    );

    let list = repo.loom(&["list"]);
    list.assert_success();
    assert_no_panic(&list);
    assert!(
        list.stdout_str().contains("vp-empty"),
        "list output should mention vp-empty:\n{}",
        list.stdout_str()
    );

    fs::write(vp.join("created_in_vp.txt"), "isolated\n").expect("write inside vp");
    assert!(
        !repo.exists("created_in_vp.txt"),
        "viewport write leaked to trunk"
    );

    let drop_out = repo.loom_in(&vp, &["drop", "--reason", "empty repo cleanup"]);
    drop_out.assert_success();
    assert_no_panic(&drop_out);

    let gc_out = repo.loom(&["gc", "--prune-viewports"]);
    gc_out.assert_success();
    assert_no_panic(&gc_out);
    assert!(!vp.exists(), "vp-empty should be removed after gc");
}

#[test]
fn tier2_f08_deeply_nested_paths_and_unicode_filenames() {
    let repo = TestRepo::new().expect("create TestRepo");

    let deep_rel = "d01/d02/d03/d04/d05/d06/d07/d08/d09/d10/leaf_module.rs";
    let deep_content = "pub fn deep_leaf() -> u32 { 42 }\n";
    let uni_rel = "src/모듈_한글/café naïve/데이터_αβγ.rs";
    let uni_content = "// 주석 café αβγ 日本語\npub fn greet() -> &'static str { \"안녕世界\" }\n";
    let doc_rel = "docs/résumé_문서/sp ace/note—v1.txt";
    let doc_content = "UTF-8 body: π≈3.14159 — 한글\n";

    repo.write_file(deep_rel, deep_content).expect("write deep_rel");
    repo.write_file(uni_rel, uni_content).expect("write uni_rel");
    repo.write_file(doc_rel, doc_content).expect("write doc_rel");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    let spawn = repo.loom(&["spawn", "-n", "vp-deep", "-m", "deep unicode paths"]);
    spawn.assert_success();
    assert_no_panic(&spawn);

    let vp = repo.viewport_dir("vp-deep");
    assert_eq!(
        fs::read_to_string(vp.join(deep_rel)).expect("read vp deep_rel"),
        deep_content
    );
    assert_eq!(
        fs::read_to_string(vp.join(uni_rel)).expect("read vp uni_rel"),
        uni_content
    );
    assert_eq!(
        fs::read_to_string(vp.join(doc_rel)).expect("read vp doc_rel"),
        doc_content
    );

    overwrite_in_place(
        &vp.join(uni_rel),
        b"// modified\npub fn greet() -> &'static str { \"\xEB\xB3\x80\xEA\xB2\xBD\xEB\x90\xA8\" }\n",
    );
    let new_uni_rel = "d01/d02/d03/d04/d05/새_파일.txt";
    fs::write(vp.join(new_uni_rel), "new\n").expect("write new unicode file in vp");

    assert_eq!(
        repo.read_file(uni_rel).expect("read trunk uni_rel"),
        uni_content
    );
    assert!(!repo.exists(new_uni_rel));

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_panic(&status);

    let drop_out = repo.loom_in(&vp, &["drop", "--reason", "deep path restore"]);
    drop_out.assert_success();
    assert_no_panic(&drop_out);

    let gc_out = repo.loom(&["gc", "--prune-viewports"]);
    gc_out.assert_success();
    assert_no_panic(&gc_out);
    assert!(!vp.exists(), "vp-deep should be removed after gc");
}

#[test]
fn tier2_f08_readonly_file_attribute_handling() {
    let repo = TestRepo::new().expect("create TestRepo");
    let mut ro_guard = ReadonlyGuard::new();

    let ro_content = "pub const RO: u32 = 1;\n";
    let rw_content = "pub const RW: u32 = 2;\n";
    repo.write_file("src/ro_trunk.rs", ro_content)
        .expect("write ro_trunk");
    repo.write_file("src/rw_trunk.rs", rw_content)
        .expect("write rw_trunk");

    let ro_trunk_path = repo.path("src/ro_trunk.rs").expect("ro_trunk path");
    set_readonly_flag(&ro_trunk_path, true);
    ro_guard.track(ro_trunk_path.clone());

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    let spawn = repo.loom(&["spawn", "-n", "vp-ro", "-m", "readonly attr test"]);
    spawn.assert_success();
    assert_no_panic(&spawn);

    let vp = repo.viewport_dir("vp-ro");
    assert_eq!(
        fs::read_to_string(vp.join("src/ro_trunk.rs")).expect("read vp ro_trunk"),
        ro_content
    );

    let vp_only_ro = vp.join("src/vp_only_ro.rs");
    fs::write(&vp_only_ro, "pub const VP_RO: u32 = 3;\n").expect("write vp_only_ro");
    set_readonly_flag(&vp_only_ro, true);

    let vp_rw = vp.join("src/rw_trunk.rs");
    if let Ok(meta) = fs::metadata(&vp_rw) {
        if meta.permissions().readonly() {
            set_readonly_flag(&vp_rw, false);
        }
    }
    overwrite_in_place(&vp_rw, b"pub const RW: u32 = 99;\n");
    set_readonly_flag(&vp_rw, true);

    let drop_out = repo.loom_in(&vp, &["drop", "--reason", "readonly restore and teardown"]);
    drop_out.assert_success();
    assert_no_panic(&drop_out);

    let gc_out = repo.loom(&["gc", "--prune-viewports"]);
    gc_out.assert_success();
    assert_no_panic(&gc_out);
    assert!(!vp.exists(), "vp-ro should be removed after gc");

    set_readonly_flag(&ro_trunk_path, false);
    assert_eq!(
        repo.read_file("src/ro_trunk.rs").expect("read trunk ro"),
        ro_content
    );
    assert_eq!(
        repo.read_file("src/rw_trunk.rs").expect("read trunk rw"),
        rw_content
    );
}

#[test]
fn tier2_f08_missing_viewport_dir_on_drop_or_gc() {
    let repo = TestRepo::new().expect("create TestRepo");
    repo.write_file("src/lib.rs", "pub fn base() -> i32 { 0 }\n")
        .expect("write src/lib.rs");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    repo.loom(&["spawn", "-n", "vp-dropped-missing", "-m", "missing on gc"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp-active-missing", "-m", "missing while active"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp-dropped-missing");
    let drop_vp1 = repo.loom_in(&vp1, &["drop", "--reason", "drop before external delete"]);
    drop_vp1.assert_success();
    assert_no_panic(&drop_vp1);

    fs::remove_dir_all(&vp1).expect("externally delete dropped vp1");

    let gc1 = repo.loom(&["gc", "--prune-viewports"]);
    gc1.assert_success();
    assert_no_panic(&gc1);

    let vp2 = repo.viewport_dir("vp-active-missing");
    fs::remove_dir_all(&vp2).expect("externally delete active vp2");

    let status_after = repo.loom(&["status"]);
    status_after.assert_success();
    assert_no_panic(&status_after);

    let drop_from_root = repo.loom(&["drop", "--reason", "no cwd viewport"]);
    assert_no_panic(&drop_from_root);
    if !drop_from_root.success() {
        assert!(
            !drop_from_root.stderr_str().trim().is_empty()
                || !drop_from_root.stdout_str().trim().is_empty(),
            "expected diagnostic when drop fails from trunk root"
        );
    }

    let gc2 = repo.loom(&["gc", "--prune-viewports"]);
    assert_no_panic(&gc2);

    let status_final = repo.loom(&["status"]);
    status_final.assert_success();
    assert_no_panic(&status_final);
}

#[test]
fn tier2_f08_hidden_dotfiles_and_empty_subdirs() {
    let repo = TestRepo::new().expect("create TestRepo");
    let editorconfig = "[*.rs]\nindent_size = 4\n";
    let env_example = "LOOM_MODE=test\n";
    let tool_toml = "enabled = true\n";

    repo.write_file(".editorconfig", editorconfig)
        .expect("write .editorconfig");
    repo.write_file(".env.example", env_example)
        .expect("write .env.example");
    repo.write_file(".config/settings.toml", tool_toml)
        .expect("write .config/settings.toml");
    fs::create_dir_all(repo.path("empty_top").expect("empty_top path"))
        .expect("create empty_top");
    fs::create_dir_all(
        repo.path("nested/deep_empty/leaf")
            .expect("nested empty path"),
    )
    .expect("create nested/deep_empty/leaf");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    let spawn = repo.loom(&["spawn", "-n", "vp-dotfiles", "-m", "dotfiles and empty dirs"]);
    spawn.assert_success();
    assert_no_panic(&spawn);

    let vp = repo.viewport_dir("vp-dotfiles");
    assert_eq!(
        fs::read_to_string(vp.join(".editorconfig")).expect("read vp .editorconfig"),
        editorconfig
    );
    assert_eq!(
        fs::read_to_string(vp.join(".env.example")).expect("read vp .env.example"),
        env_example
    );
    assert_eq!(
        fs::read_to_string(vp.join(".config/settings.toml")).expect("read vp .config/settings.toml"),
        tool_toml
    );
    assert!(vp.join("empty_top").is_dir());
    assert!(vp.join("nested/deep_empty/leaf").is_dir());

    // .loom/ internals must not be recursively copied into viewport
    assert!(!vp.join(".loom").join("viewports").exists());
    assert!(!vp.join(".loom").join("state.redb").exists());
    assert!(!vp.join(".loom").join("wal").exists());

    overwrite_in_place(&vp.join(".env.example"), b"LOOM_MODE=mutated_in_vp\n");
    fs::write(vp.join(".vp_secret"), "secret\n").expect("write .vp_secret");
    fs::create_dir_all(vp.join("vp_empty_dir")).expect("create vp_empty_dir");

    assert_eq!(
        repo.read_file(".env.example").expect("read trunk .env.example"),
        env_example
    );
    assert!(!repo.exists(".vp_secret"));
    assert!(!repo.exists("vp_empty_dir"));

    let drop_out = repo.loom_in(&vp, &["drop", "--reason", "restore dotfiles"]);
    drop_out.assert_success();
    assert_no_panic(&drop_out);

    let gc_out = repo.loom(&["gc", "--prune-viewports"]);
    gc_out.assert_success();
    assert_no_panic(&gc_out);
    assert!(!vp.exists(), "vp-dotfiles should be removed after gc");

    assert!(repo.path("empty_top").expect("empty_top").is_dir());
    assert!(repo.path("nested/deep_empty/leaf").expect("leaf").is_dir());
    assert_eq!(
        repo.read_file(".editorconfig").expect("trunk .editorconfig"),
        editorconfig
    );
}

// ============================================================================
// F9: Dependency Block Sharing & Lock Isolation (5 Tier 2 Boundary Tests)
// ============================================================================

#[test]
fn tier2_f09_empty_cache_dirs_no_failure() {
    let repo = TestRepo::new().expect("create TestRepo");
    fs::create_dir_all(repo.path("target/debug/deps").expect("deps path"))
        .expect("create target/debug/deps");
    fs::create_dir_all(repo.path("node_modules").expect("node_modules path"))
        .expect("create node_modules");
    fs::create_dir_all(repo.path(".venv").expect(".venv path")).expect("create .venv");
    repo.write_file("src/lib.rs", "pub fn ok() -> bool { true }\n")
        .expect("write src/lib.rs");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    let spawn = repo.loom(&["spawn", "-n", "vp-empty-cache", "-m", "empty cache boundary"]);
    spawn.assert_success();
    assert_no_panic(&spawn);

    let vp = repo.viewport_dir("vp-empty-cache");
    assert_eq!(
        fs::read_to_string(vp.join("src/lib.rs")).expect("read vp src/lib.rs"),
        "pub fn ok() -> bool { true }\n"
    );

    fs::create_dir_all(vp.join("target")).expect("create vp target");
    fs::write(vp.join("target/.cargo-lock"), "vp-lock").expect("write vp target/.cargo-lock");

    assert!(!repo.exists("target/.cargo-lock"));
    for empty_dir in ["node_modules", ".venv", "target/debug/deps"] {
        let count = fs::read_dir(repo.path(empty_dir).expect("empty cache dir"))
            .expect("read_dir trunk cache dir")
            .count();
        assert_eq!(count, 0, "trunk {empty_dir} should remain empty");
    }

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_panic(&status);

    let drop_out = repo.loom_in(&vp, &["drop", "--reason", "cleanup empty cache"]);
    drop_out.assert_success();
    assert_no_panic(&drop_out);

    let gc_out = repo.loom(&["gc", "--prune-viewports"]);
    gc_out.assert_success();
    assert_no_panic(&gc_out);
    assert!(!vp.exists(), "vp-empty-cache should be removed after gc");
}

#[test]
fn tier2_f09_mutating_file_inside_viewport_src_never_leaks() {
    let repo = TestRepo::new().expect("create TestRepo");
    let cargo_toml = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";
    let lib_rs = "pub fn v() -> i32 { 1 }\n";
    let helper_rs = "pub fn h() -> i32 { 2 }\n";

    repo.write_file("Cargo.toml", cargo_toml)
        .expect("write Cargo.toml");
    repo.write_file("src/lib.rs", lib_rs)
        .expect("write src/lib.rs");
    repo.write_file("src/sub/helper.rs", helper_rs)
        .expect("write src/sub/helper.rs");
    repo.write_file("node_modules/pkg/index.js", "module.exports = 1;\n")
        .expect("write node_modules");
    repo.write_file(".venv/pyvenv.cfg", "home = /usr/bin\n")
        .expect("write .venv");
    repo.write_bytes("target/debug/deps/libdemo-0123.rlib", b"!<arch>\n")
        .expect("write rlib");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    repo.loom(&["spawn", "-n", "vp1", "-m", "mutate src"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "observe isolation"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    overwrite_in_place(&vp1.join("src/lib.rs"), b"pub fn v() -> i32 { 999 }\n");
    overwrite_in_place(
        &vp1.join("Cargo.toml"),
        b"[package]\nname = \"mutated\"\nversion = \"0.9.9\"\n",
    );
    fs::write(vp1.join("src/sub/added.rs"), "pub fn added() {}\n").expect("write added.rs");
    fs::remove_file(vp1.join("src/sub/helper.rs")).expect("remove helper.rs in vp1");

    // Trunk remains byte-for-byte untouched
    assert_eq!(repo.read_file("src/lib.rs").expect("trunk lib.rs"), lib_rs);
    assert_eq!(
        repo.read_file("Cargo.toml").expect("trunk Cargo.toml"),
        cargo_toml
    );
    assert_eq!(
        repo.read_file("src/sub/helper.rs").expect("trunk helper.rs"),
        helper_rs
    );
    assert!(!repo.exists("src/sub/added.rs"));

    // vp2 remains byte-for-byte untouched
    assert_eq!(
        fs::read_to_string(vp2.join("src/lib.rs")).expect("vp2 lib.rs"),
        lib_rs
    );
    assert_eq!(
        fs::read_to_string(vp2.join("Cargo.toml")).expect("vp2 Cargo.toml"),
        cargo_toml
    );
    assert_eq!(
        fs::read_to_string(vp2.join("src/sub/helper.rs")).expect("vp2 helper.rs"),
        helper_rs
    );
    assert!(!vp2.join("src/sub/added.rs").exists());

    let drop1 = repo.loom_in(&vp1, &["drop", "--reason", "check restore_clean isolation"]);
    drop1.assert_success();
    assert_no_panic(&drop1);

    assert_eq!(repo.read_file("src/lib.rs").expect("trunk lib.rs"), lib_rs);
    assert_eq!(
        fs::read_to_string(vp2.join("src/lib.rs")).expect("vp2 lib.rs after vp1 drop"),
        lib_rs
    );

    let drop2 = repo.loom_in(&vp2, &["drop", "--reason", "cleanup vp2"]);
    drop2.assert_success();
    assert_no_panic(&drop2);

    let gc = repo.loom(&["gc", "--prune-viewports"]);
    gc.assert_success();
    assert_no_panic(&gc);
    assert!(!vp1.exists() && !vp2.exists());
}

#[test]
fn tier2_f09_locked_file_in_vp1_does_not_block_vp2_spawn() {
    // Declare `repo` BEFORE any File handles so reverse Drop order releases locks first on panic.
    let repo = TestRepo::new().expect("create TestRepo");
    let lib_content = "pub fn run() -> i32 { 7 }\n";

    repo.write_file("src/lib.rs", lib_content)
        .expect("write src/lib.rs");
    repo.write_file("target/.cargo-lock", "trunk-root-lock")
        .expect("write target/.cargo-lock");
    repo.write_file("target/debug/.cargo-lock", "trunk-debug-lock")
        .expect("write target/debug/.cargo-lock");
    repo.write_bytes("target/debug/deps/libcached-1111.rlib", b"!<arch>\ncached")
        .expect("write cached rlib");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    repo.loom(&["spawn", "-n", "vp1", "-m", "lock holder"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    {
        let lock_vp1_root = open_exclusive_lock(&vp1.join("target/.cargo-lock"))
            .expect("acquire exclusive lock on vp1 target/.cargo-lock");
        let lock_vp1_debug = open_exclusive_lock(&vp1.join("target/debug/.cargo-lock"))
            .expect("acquire exclusive lock on vp1 target/debug/.cargo-lock");
        let lock_vp1_src = open_exclusive_lock(&vp1.join("src/lib.rs"))
            .expect("acquire exclusive lock on vp1 src/lib.rs");

        // Spawn vp2 while vp1 holds exclusive OS file locks
        let spawn2 = repo.loom(&["spawn", "-n", "vp2", "-m", "concurrent spawn under lock"]);
        spawn2.assert_success();
        assert_no_panic(&spawn2);

        assert_eq!(
            fs::read_to_string(vp2.join("src/lib.rs")).expect("read vp2 src/lib.rs"),
            lib_content
        );

        let lock_vp2_root = open_exclusive_lock(&vp2.join("target/.cargo-lock"))
            .expect("vp2 target/.cargo-lock must be exclusively lockable while vp1 holds lock");
        let lock_vp2_debug = open_exclusive_lock(&vp2.join("target/debug/.cargo-lock"))
            .expect("vp2 target/debug/.cargo-lock must be exclusively lockable while vp1 holds lock");

        fs::create_dir_all(vp2.join("target/debug")).expect("create vp2 target/debug");
        fs::write(vp2.join("target/debug/output.exe"), b"MZ_VP2_BIN")
            .expect("write vp2 output binary");

        let lock_trunk_root = open_exclusive_lock(&repo.path("target/.cargo-lock").unwrap())
            .expect("trunk target/.cargo-lock must remain isolated from vp1/vp2");

        drop(lock_trunk_root);
        drop(lock_vp2_debug);
        drop(lock_vp2_root);
        drop(lock_vp1_src);
        drop(lock_vp1_debug);
        drop(lock_vp1_root);
    }

    assert_eq!(
        repo.read_file("target/.cargo-lock")
            .expect("read trunk target/.cargo-lock"),
        "trunk-root-lock"
    );
    assert!(!repo.exists("target/debug/output.exe"));

    repo.loom_in(&vp1, &["drop", "--reason", "release vp1"])
        .assert_success();
    repo.loom_in(&vp2, &["drop", "--reason", "release vp2"])
        .assert_success();
    let gc = repo.loom(&["gc", "--prune-viewports"]);
    gc.assert_success();
    assert_no_panic(&gc);
    assert!(!vp1.exists() && !vp2.exists());
}

#[test]
fn tier2_f09_corrupt_or_missing_rlib_handled_cleanly() {
    let repo = TestRepo::new().expect("create TestRepo");
    let lib_content = "pub fn rlib_boundary() -> u8 { 1 }\n";

    repo.write_file("src/lib.rs", lib_content)
        .expect("write src/lib.rs");
    repo.write_bytes("target/debug/deps/libzero-0000.rlib", b"")
        .expect("write 0-byte rlib");
    repo.write_bytes(
        "target/debug/deps/libcorrupt-dead.rlib",
        b"\x00\xFF\xFEBAD_RLIB_GARBAGE",
    )
    .expect("write corrupt rlib");
    repo.write_bytes(
        "target/debug/deps/libvalid-1111.rlib",
        b"!<arch>\n/               0\n",
    )
    .expect("write valid-like rlib");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    let spawn1 = repo.loom(&["spawn", "-n", "vp1", "-m", "corrupt rlib spawn"]);
    spawn1.assert_success();
    assert_no_panic(&spawn1);

    let vp1 = repo.viewport_dir("vp1");
    assert_eq!(
        fs::read_to_string(vp1.join("src/lib.rs")).expect("read vp1 src/lib.rs"),
        lib_content
    );

    let vp1_valid_rlib = vp1.join("target/debug/deps/libvalid-1111.rlib");
    if vp1_valid_rlib.exists() {
        fs::remove_file(&vp1_valid_rlib).expect("remove vp1 valid rlib");
    }
    fs::remove_file(
        repo.path("target/debug/deps/libzero-0000.rlib")
            .expect("trunk zero rlib path"),
    )
    .expect("remove trunk zero rlib");

    let spawn2 = repo.loom(&["spawn", "-n", "vp2", "-m", "spawn after rlib removal"]);
    spawn2.assert_success();
    assert_no_panic(&spawn2);

    let vp2 = repo.viewport_dir("vp2");
    assert_eq!(
        fs::read_to_string(vp2.join("src/lib.rs")).expect("read vp2 src/lib.rs"),
        lib_content
    );

    let drop1 = repo.loom_in(&vp1, &["drop", "--reason", "drop with missing rlib"]);
    drop1.assert_success();
    assert_no_panic(&drop1);

    let drop2 = repo.loom_in(&vp2, &["drop", "--reason", "drop vp2"]);
    drop2.assert_success();
    assert_no_panic(&drop2);

    let gc = repo.loom(&["gc", "--prune-viewports"]);
    gc.assert_success();
    assert_no_panic(&gc);
    assert!(!vp1.exists() && !vp2.exists());

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_panic(&status);
}

#[test]
fn tier2_f09_special_lockfile_names_isolated() {
    let repo = TestRepo::new().expect("create TestRepo");
    repo.write_file("src/main.rs", "fn main() {}\n")
        .expect("write src/main.rs");

    let cargo_locks = [
        ("target/.cargo-lock", "trunk-t"),
        ("target/debug/.cargo-lock", "trunk-td"),
        ("target/release/.cargo-lock", "trunk-tr"),
        (
            "target/x86_64-pc-windows-msvc/debug/.cargo-lock",
            "trunk-triple",
        ),
    ];
    for (rel, content) in cargo_locks {
        repo.write_file(rel, content).expect("write .cargo-lock");
    }

    let workspace_locks = [
        ("Cargo.lock", "# trunk Cargo.lock\nversion = 3\n"),
        ("package-lock.json", "{\"name\":\"trunk\",\"lockfileVersion\":3}\n"),
        ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        ("uv.lock", "version = 1\n"),
        (".custom.lock", "trunk-dotlock\n"),
    ];
    for (rel, content) in workspace_locks {
        repo.write_file(rel, content)
            .expect("write workspace lockfile");
    }

    repo.write_file("node_modules/left-pad/index.js", "module.exports = 1;\n")
        .expect("write node_modules");
    repo.write_file(".venv/pyvenv.cfg", "home = /usr/bin\n")
        .expect("write .venv");

    let init = repo.loom(&["init"]);
    init.assert_success();
    assert_no_panic(&init);

    repo.loom(&["spawn", "-n", "vp1", "-m", "lockfiles vp1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "lockfiles vp2"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    for (rel, _) in cargo_locks {
        let p = vp1.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("create target parent in vp1");
        }
        if p.exists() {
            overwrite_in_place(&p, b"vp1-modified");
        } else {
            fs::write(&p, b"vp1-modified").expect("write vp1 .cargo-lock");
        }
    }

    for (rel, trunk_content) in workspace_locks {
        assert_eq!(
            fs::read_to_string(vp1.join(rel)).expect("read vp1 workspace lock"),
            trunk_content
        );
        assert_eq!(
            fs::read_to_string(vp2.join(rel)).expect("read vp2 workspace lock"),
            trunk_content
        );
        overwrite_in_place(&vp1.join(rel), b"vp1-lock-override");
    }

    // Verify trunk & vp2 were unaffected by vp1 in-place writes
    for (rel, trunk_content) in cargo_locks {
        assert_eq!(
            repo.read_file(rel).expect("read trunk .cargo-lock"),
            trunk_content
        );
        let vp2_p = vp2.join(rel);
        if vp2_p.exists() {
            assert_ne!(
                fs::read_to_string(&vp2_p).expect("read vp2 .cargo-lock"),
                "vp1-modified"
            );
        }
    }
    for (rel, trunk_content) in workspace_locks {
        assert_eq!(
            repo.read_file(rel).expect("read trunk workspace lock"),
            trunk_content
        );
        assert_eq!(
            fs::read_to_string(vp2.join(rel)).expect("read vp2 workspace lock"),
            trunk_content
        );
    }

    // Mutate vp2 locks independently and ensure trunk remains untouched
    let vp2_t_lock = vp2.join("target/.cargo-lock");
    if let Some(parent) = vp2_t_lock.parent() {
        fs::create_dir_all(parent).expect("create vp2 target dir");
    }
    fs::write(&vp2_t_lock, b"vp2-modified").expect("write vp2 target/.cargo-lock");
    overwrite_in_place(&vp2.join("Cargo.lock"), b"vp2-modified");
    assert_eq!(
        repo.read_file("target/.cargo-lock").expect("trunk target/.cargo-lock"),
        "trunk-t"
    );
    assert_eq!(
        repo.read_file("Cargo.lock").expect("trunk Cargo.lock"),
        "# trunk Cargo.lock\nversion = 3\n"
    );

    repo.loom_in(&vp1, &["drop", "--reason", "cleanup vp1"])
        .assert_success();
    repo.loom_in(&vp2, &["drop", "--reason", "cleanup vp2"])
        .assert_success();
    let gc = repo.loom(&["gc", "--prune-viewports"]);
    gc.assert_success();
    assert_no_panic(&gc);
    assert!(!vp1.exists() && !vp2.exists());
}

// ============================================================================
// F10: Viewport Lifecycle & `loom spawn` (5 Tier 2 Boundary Tests)
// ============================================================================

#[test]
fn tier2_f10_spawn_duplicate_name_rejected() {
    let repo = TestRepo::new().expect("create TestRepo");
    let orig = "pub fn original() -> i32 { 1 }\n";
    repo.write_file("src/lib.rs", orig).expect("write src/lib.rs");

    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "dup-vp", "-m", "first intent"])
        .assert_success();

    let vp_dir = repo.viewport_dir("dup-vp");
    let modified = "pub fn modified_in_vp1() -> i32 { 99 }\n";
    overwrite_in_place(&vp_dir.join("src/lib.rs"), modified.as_bytes());

    let dup = repo.loom(&["spawn", "-n", "dup-vp", "-m", "duplicate intent"]);
    assert_clean_failure(&dup);

    assert!(vp_dir.is_dir(), "existing dup-vp directory must remain");
    assert_eq!(
        fs::read_to_string(vp_dir.join("src/lib.rs")).expect("read dup-vp src/lib.rs"),
        modified,
        "duplicate spawn must not overwrite existing viewport edits"
    );
    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), orig);

    let list = repo.loom(&["list"]);
    list.assert_success();
    assert_no_panic(&list);
    assert!(list.stdout_str().contains("dup-vp"));

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_panic(&status);

    let other = repo.loom(&["spawn", "-n", "other-vp", "-m", "valid"]);
    other.assert_success();
    assert_no_panic(&other);
    assert!(repo.viewport_dir("other-vp").is_dir());
}

#[test]
fn tier2_f10_spawn_invalid_path_traversal_name() {
    let repo = TestRepo::new().expect("create TestRepo");
    let workspace = repo.path("workspace").expect("workspace path");
    fs::create_dir_all(&workspace).expect("create workspace");
    repo.write_file("workspace/README.md", "sentinel\n")
        .expect("write workspace README");

    repo.loom_in(&workspace, &["init"]).assert_success();

    let mut invalid_names = vec![
        "",
        ".",
        "..",
        "../escape",
        "../../escape2",
        "../../../outside_escape",
        r"..\win_escape",
        r"..\..\win_escape2",
        r"..\..\..\outside_win",
        "nested/sub",
        r"nested\sub",
        "/abs_escape",
    ];
    if cfg!(windows) {
        invalid_names.push(r"C:\abs_win");
        invalid_names.push("C:drive_rel");
    }

    for bad in invalid_names {
        let out = repo.loom_in(&workspace, &["spawn", "-n", bad, "-m", "traversal test"]);
        assert_clean_failure(&out);
    }

    let vp_root = workspace.join(".loom").join("viewports");
    if vp_root.exists() {
        let entries = fs::read_dir(&vp_root).expect("read viewports dir").count();
        assert_eq!(
            entries, 0,
            "invalid viewport names must not create directories under .loom/viewports"
        );
    }

    for escaped in [
        workspace.join(".loom").join("escape"),
        workspace.join("escape2"),
        workspace.join(".loom").join("win_escape"),
        workspace.join("win_escape2"),
        repo.root().join("outside_escape"),
        repo.root().join("outside_win"),
    ] {
        assert!(
            !escaped.exists(),
            "traversal escaped to {}",
            escaped.display()
        );
    }

    let list = repo.loom_in(&workspace, &["list"]);
    list.assert_success();
    assert_no_panic(&list);

    let valid = repo.loom_in(&workspace, &["spawn", "-n", "safe-vp", "-m", "valid"]);
    valid.assert_success();
    assert_no_panic(&valid);
    assert!(vp_root.join("safe-vp").is_dir());
}

#[test]
fn tier2_f10_spawn_wrapped_command_nonzero_exit() {
    let repo = TestRepo::new().expect("create TestRepo");
    let base = "pub fn base() -> i32 { 1 }\n";
    repo.write_file("src/lib.rs", base).expect("write src/lib.rs");

    repo.loom(&["init"]).assert_success();

    #[cfg(windows)]
    let args = [
        "spawn",
        "-n",
        "vp-fail",
        "-m",
        "failing wrapped cmd",
        "--",
        "cmd",
        "/C",
        "exit",
        "42",
    ];
    #[cfg(not(windows))]
    let args = [
        "spawn",
        "-n",
        "vp-fail",
        "-m",
        "failing wrapped cmd",
        "--",
        "sh",
        "-c",
        "exit 42",
    ];

    let out = repo.loom(&args);
    out.assert_failure();
    assert_no_panic(&out);

    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), base);

    let list = repo.loom(&["list"]);
    list.assert_success();
    assert_no_panic(&list);

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_panic(&status);

    let vp_dir = repo.viewport_dir("vp-fail");
    if vp_dir.is_dir() {
        assert!(
            list.stdout_str().contains("vp-fail"),
            "if vp-fail dir exists, list must reflect it"
        );
        let drop_out = repo.loom_in(&vp_dir, &["drop", "--reason", "wrapped cmd failed"]);
        drop_out.assert_success();
        assert_no_panic(&drop_out);
    } else {
        assert!(
            !list.stdout_str().contains("vp-fail"),
            "if vp-fail was rolled back on disk, list must not keep a dangling entry"
        );
    }

    let after = repo.loom(&["spawn", "-n", "vp-after", "-m", "healthy"]);
    after.assert_success();
    assert_no_panic(&after);
    assert!(repo.viewport_dir("vp-after").is_dir());
}

#[test]
fn tier2_f10_spawn_nonexistent_wrapped_command() {
    let repo = TestRepo::new().expect("create TestRepo");
    let base = "pub fn base() -> i32 { 1 }\n";
    repo.write_file("src/lib.rs", base).expect("write src/lib.rs");

    repo.loom(&["init"]).assert_success();

    let out = repo.loom(&[
        "spawn",
        "-n",
        "vp-missing-cmd",
        "-m",
        "missing cmd",
        "--",
        "loom_nonexistent_wrapped_cmd_xyz_99999",
    ]);
    assert_clean_failure(&out);

    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), base);

    let list = repo.loom(&["list"]);
    list.assert_success();
    assert_no_panic(&list);

    let status = repo.loom(&["status"]);
    status.assert_success();
    assert_no_panic(&status);

    let vp_dir = repo.viewport_dir("vp-missing-cmd");
    if vp_dir.is_dir() {
        assert!(
            list.stdout_str().contains("vp-missing-cmd"),
            "if vp-missing-cmd dir exists, list must reflect it"
        );
        let drop_out = repo.loom_in(&vp_dir, &["drop", "--reason", "missing cmd cleanup"]);
        drop_out.assert_success();
        assert_no_panic(&drop_out);
    } else {
        assert!(
            !list.stdout_str().contains("vp-missing-cmd"),
            "if vp-missing-cmd was rolled back on disk, list must not keep a dangling entry"
        );
    }

    let healthy = repo.loom(&["spawn", "-n", "vp-healthy", "-m", "valid"]);
    healthy.assert_success();
    assert_no_panic(&healthy);
    assert!(repo.viewport_dir("vp-healthy").is_dir());
}

#[test]
fn tier2_f10_invalid_lifecycle_transition_seal_dropped() {
    let repo = TestRepo::new().expect("create TestRepo");
    let stable = "pub fn stable() -> i32 { 10 }\n";
    repo.write_file("src/lib.rs", stable)
        .expect("write src/lib.rs");

    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-drop-first", "-m", "experimental work"])
        .assert_success();

    let vp_dir = repo.viewport_dir("vp-drop-first");
    overwrite_in_place(
        &vp_dir.join("src/lib.rs"),
        b"pub fn bad_attempt() -> i32 { -1 }\n",
    );

    let drop_out = repo.loom_in(&vp_dir, &["drop", "--reason", "dead-end lifecycle test"]);
    drop_out.assert_success();
    assert_no_panic(&drop_out);
    assert!(
        vp_dir.is_dir(),
        "dropped viewport directory should remain until gc"
    );

    let list_before = repo.loom(&["list"]);
    list_before.assert_success();
    assert_no_panic(&list_before);
    let before_lines = lines_for_viewport(&list_before.stdout_str(), "vp-drop-first");
    assert!(
        before_lines
            .iter()
            .any(|l| l.to_ascii_lowercase().contains("dropped")),
        "expected vp-drop-first to show dropped state in list:\n{}",
        list_before.stdout_str()
    );

    let seal_out = repo.loom_in(
        &vp_dir,
        &["seal", "-m", "must reject sealing dropped viewport"],
    );
    assert_clean_failure(&seal_out);

    let list_after = repo.loom(&["list"]);
    list_after.assert_success();
    assert_no_panic(&list_after);
    let list_after_lines = lines_for_viewport(&list_after.stdout_str(), "vp-drop-first");
    assert!(
        !list_after_lines.is_empty(),
        "vp-drop-first should still be listed after rejected seal:\n{}",
        list_after.stdout_str()
    );
    for line in &list_after_lines {
        let lower = line.to_ascii_lowercase();
        assert!(
            lower.contains("dropped"),
            "expected dropped state to remain on line: {line}"
        );
        assert!(
            !has_word(&lower, "sealed") && !has_word(&lower, "active"),
            "rejected seal must not transition vp-drop-first to sealed/active: {line}"
        );
    }

    let status_after = repo.loom(&["status"]);
    status_after.assert_success();
    assert_no_panic(&status_after);
    for line in lines_for_viewport(&status_after.stdout_str(), "vp-drop-first") {
        let lower = line.to_ascii_lowercase();
        assert!(
            !has_word(&lower, "sealed") && !has_word(&lower, "active"),
            "status must not show vp-drop-first as sealed/active: {line}"
        );
    }

    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), stable);

    let cap_log = repo.loom(&["log", "--capsules"]);
    cap_log.assert_success();
    assert_no_panic(&cap_log);
    assert!(
        !cap_log
            .stdout_str()
            .contains("must reject sealing dropped viewport"),
        "rejected seal must not create a CausalCapsule"
    );

    let gc_out = repo.loom(&["gc", "--prune-viewports"]);
    gc_out.assert_success();
    assert_no_panic(&gc_out);
    assert!(!vp_dir.exists(), "vp-drop-first should be pruned by gc");
}
