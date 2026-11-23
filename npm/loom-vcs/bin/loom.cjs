#!/usr/bin/env node
"use strict";

const childProcess = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const PLATFORM_PACKAGES = Object.freeze({
  "darwin-arm64": Object.freeze({
    packageName: "loom-vcs-darwin-arm64",
    binName: "loom",
  }),
  "darwin-x64": Object.freeze({
    packageName: "loom-vcs-darwin-x64",
    binName: "loom",
  }),
  "linux-x64": Object.freeze({
    packageName: "loom-vcs-linux-x64",
    binName: "loom",
  }),
  "linux-arm64": Object.freeze({
    packageName: "loom-vcs-linux-arm64",
    binName: "loom",
  }),
  "win32-x64": Object.freeze({
    packageName: "loom-vcs-win32-x64",
    binName: "loom.exe",
  }),
  "win32-arm64": Object.freeze({
    packageName: "loom-vcs-win32-arm64",
    binName: "loom.exe",
  }),
});

function formatMissingBinaryMessage(entry, cause) {
  const detail = cause && cause.message ? ` (${cause.message})` : "";
  return [
    `loom-vcs: unable to locate the native binary package "${entry.packageName}"${detail}.`,
    `If you installed with "--no-optional" or "--omit=optional", re-install without skipping optionalDependencies:`,
    `  npm install -g loom-vcs`,
    `Or install via another distribution channel:`,
    `  brew install wick-lim/tap/loom-vcs`,
    `  cargo install loom-vcs`,
    `  https://github.com/Wick-Lim/loom/releases`,
  ].join("\n");
}

function resolveBinaryPath(options = {}) {
  const env = options.env || process.env;
  const platform = options.platform || process.platform;
  const arch = options.arch || process.arch;
  const resolveFn = options.resolveFn || require.resolve;
  const existsSync = options.existsSync || fs.existsSync;

  const customRaw = typeof env.LOOM_BINARY_PATH === "string" ? env.LOOM_BINARY_PATH.trim() : "";
  if (customRaw.length > 0) {
    const customPath = path.resolve(customRaw);
    if (!existsSync(customPath)) {
      throw new Error(`loom-vcs: LOOM_BINARY_PATH points to missing binary: ${customPath}`);
    }
    return { binaryPath: customPath, source: "env" };
  }

  const key = `${platform}-${arch}`;
  const entry = PLATFORM_PACKAGES[key];
  if (!entry) {
    const supported = Object.keys(PLATFORM_PACKAGES).join(", ");
    throw new Error(
      [
        `loom-vcs: unsupported platform/architecture "${key}".`,
        `Supported npm binary targets: ${supported}.`,
        `Try building from source or downloading from releases:`,
        `  cargo install loom-vcs`,
        `  https://github.com/Wick-Lim/loom/releases`,
      ].join("\n")
    );
  }

  const targetRequest = `${entry.packageName}/bin/${entry.binName}`;
  let binaryPath;
  try {
    binaryPath = resolveFn(targetRequest);
  } catch (err) {
    throw new Error(formatMissingBinaryMessage(entry, err));
  }

  if (!existsSync(binaryPath)) {
    throw new Error(formatMissingBinaryMessage(entry, new Error(`missing file at ${binaryPath}`)));
  }

  return { binaryPath, source: entry.packageName };
}

function ensureExecutable(binaryPath) {
  if (process.platform === "win32") {
    return;
  }
  try {
    fs.accessSync(binaryPath, fs.constants.X_OK);
  } catch {
    try {
      fs.chmodSync(binaryPath, 0o755);
    } catch {
      // Ignore permission errors on read-only filesystems (e.g. Nix/containers).
    }
  }
}

function main() {
  let resolved;
  try {
    resolved = resolveBinaryPath();
  } catch (err) {
    process.stderr.write(`${err.message}\n`);
    process.exit(1);
  }

  ensureExecutable(resolved.binaryPath);

  const result = childProcess.spawnSync(resolved.binaryPath, process.argv.slice(2), {
    stdio: "inherit",
    env: process.env,
  });

  if (result.error) {
    process.stderr.write(`loom-vcs: failed to launch ${resolved.binaryPath}: ${result.error.message}\n`);
    process.exit(1);
  }

  if (result.signal) {
    try {
      process.kill(process.pid, result.signal);
    } catch {
      process.exit(1);
    }
    process.exit(1);
  }

  process.exit(result.status === null ? 1 : result.status);
}

module.exports = {
  PLATFORM_PACKAGES,
  formatMissingBinaryMessage,
  resolveBinaryPath,
};

if (require.main === module) {
  main();
}
