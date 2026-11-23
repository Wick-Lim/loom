import test from "node:test";
import assert from "node:assert/strict";
import childProcess from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const __dirname = path.dirname(fileURLToPath(import.meta.url));
const launcherPath = path.resolve(__dirname, "../bin/loom.cjs");
const repoRoot = path.resolve(__dirname, "../../..");
const { PLATFORM_PACKAGES, resolveBinaryPath } = require(launcherPath);

test("maps all 6 supported platform/arch combinations to expected packages", () => {
  const expected = {
    "darwin-arm64": "loom-vcs-darwin-arm64/bin/loom",
    "darwin-x64": "loom-vcs-darwin-x64/bin/loom",
    "linux-x64": "loom-vcs-linux-x64/bin/loom",
    "linux-arm64": "loom-vcs-linux-arm64/bin/loom",
    "win32-x64": "loom-vcs-win32-x64/bin/loom.exe",
    "win32-arm64": "loom-vcs-win32-arm64/bin/loom.exe",
  };

  assert.deepEqual(Object.keys(PLATFORM_PACKAGES).sort(), Object.keys(expected).sort());

  for (const [key, reqPath] of Object.entries(expected)) {
    const [platform, arch] = key.split("-");
    const fakePath = `/virtual/node_modules/${reqPath}`;
    const res = resolveBinaryPath({
      env: {},
      platform,
      arch,
      resolveFn: (id) => {
        assert.equal(id, reqPath);
        return fakePath;
      },
      existsSync: (p) => p === fakePath,
    });
    assert.equal(res.binaryPath, fakePath);
    assert.equal(res.source, PLATFORM_PACKAGES[key].packageName);
  }
});

test("rejects unsupported platform/arch with helpful fallback instructions", () => {
  assert.throws(
    () => resolveBinaryPath({ env: {}, platform: "freebsd", arch: "x64" }),
    (err) => {
      assert.match(err.message, /unsupported platform\/architecture "freebsd-x64"/);
      assert.match(err.message, /darwin-arm64/);
      assert.match(err.message, /cargo install loom-vcs/);
      return true;
    }
  );
});

test("reports clear guidance when optional dependency package is missing", () => {
  assert.throws(
    () =>
      resolveBinaryPath({
        env: {},
        platform: "linux",
        arch: "x64",
        resolveFn: () => {
          throw new Error("Cannot find module 'loom-vcs-linux-x64/bin/loom'");
        },
      }),
    (err) => {
      assert.match(err.message, /loom-vcs-linux-x64/);
      assert.match(err.message, /--no-optional/);
      assert.match(err.message, /brew install wick-lim\/tap\/loom-vcs/);
      assert.match(err.message, /cargo install loom-vcs/);
      return true;
    }
  );
});

test("fails loudly when LOOM_BINARY_PATH points to a nonexistent path", () => {
  assert.throws(
    () =>
      resolveBinaryPath({
        env: { LOOM_BINARY_PATH: path.join(repoRoot, "target", "nonexistent-loom-bin") },
        existsSync: () => false,
      }),
    (err) => {
      assert.match(err.message, /LOOM_BINARY_PATH points to missing binary/);
      return true;
    }
  );
});

function findLocalLoomBinary() {
  if (process.env.LOOM_BINARY_PATH && fs.existsSync(process.env.LOOM_BINARY_PATH)) {
    return process.env.LOOM_BINARY_PATH;
  }
  const exe = process.platform === "win32" ? "loom.exe" : "loom";
  for (const profile of ["debug", "release"]) {
    const candidate = path.join(repoRoot, "target", profile, exe);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  return null;
}

test("executes real loom binary via LOOM_BINARY_PATH override (--version and --help)", () => {
  const binPath = findLocalLoomBinary();
  assert.ok(
    binPath,
    "expected a compiled loom binary at target/debug/loom(.exe), target/release/loom(.exe), or LOOM_BINARY_PATH"
  );

  const verRes = childProcess.spawnSync(process.execPath, [launcherPath, "--version"], {
    env: { ...process.env, LOOM_BINARY_PATH: binPath },
    encoding: "utf8",
  });
  assert.equal(verRes.status, 0, `stderr: ${verRes.stderr}`);
  assert.match(verRes.stdout, /^loom\s+\d+\.\d+\.\d+/);

  const helpRes = childProcess.spawnSync(process.execPath, [launcherPath, "--help"], {
    env: { ...process.env, LOOM_BINARY_PATH: binPath },
    encoding: "utf8",
  });
  assert.equal(helpRes.status, 0, `stderr: ${helpRes.stderr}`);
  assert.match(helpRes.stdout, /Project Loom/);
});
