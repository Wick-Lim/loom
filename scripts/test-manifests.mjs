import test from "node:test";
import assert from "node:assert/strict";
import childProcess from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const generatorPath = path.resolve(__dirname, "generate-manifests.mjs");

function makeSampleSums(version) {
  const entries = [
    ["aarch64-apple-darwin.tar.gz", "1".repeat(64)],
    ["x86_64-apple-darwin.tar.gz", "2".repeat(64)],
    ["x86_64-unknown-linux-gnu.tar.gz", "3".repeat(64)],
    ["aarch64-unknown-linux-gnu.tar.gz", "4".repeat(64)],
    ["x86_64-pc-windows-msvc.zip", "5".repeat(64)],
    ["aarch64-pc-windows-msvc.zip", "6".repeat(64)],
  ];
  return entries
    .map(([suffix, sha]) => `${sha}  loom-vcs-v${version}-${suffix}`)
    .join("\n") + "\n";
}

test("generates Homebrew, Scoop, and Winget manifests from SHA256SUMS.txt", () => {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "loom-manifest-test-"));
  try {
    const version = "1.2.3";
    const checksumsPath = path.join(tmpDir, "SHA256SUMS.txt");
    fs.writeFileSync(checksumsPath, makeSampleSums(version), "utf8");

    const res = childProcess.spawnSync(
      process.execPath,
      [generatorPath, "--version", `v${version}`, "--checksums", checksumsPath, "--out-root", tmpDir],
      { encoding: "utf8" }
    );
    assert.equal(res.status, 0, `stderr: ${res.stderr}`);

    const formula = fs.readFileSync(path.join(tmpDir, "Formula", "loom-vcs.rb"), "utf8");
    assert.match(formula, /class LoomVcs < Formula/);
    assert.match(formula, /version "1\.2\.3"/);
    assert.match(formula, new RegExp(`sha256 "${"1".repeat(64)}"`));
    assert.match(formula, new RegExp(`sha256 "${"3".repeat(64)}"`));
    assert.match(formula, /bin\.install "loom"/);

    const scoop = JSON.parse(
      fs.readFileSync(path.join(tmpDir, "packaging", "scoop", "loom-vcs.json"), "utf8")
    );
    assert.equal(scoop.version, "1.2.3");
    assert.equal(scoop.bin, "loom.exe");
    assert.equal(scoop.architecture["64bit"].hash, "5".repeat(64));
    assert.equal(scoop.architecture.arm64.hash, "6".repeat(64));
    assert.match(scoop.architecture["64bit"].url, /releases\/download\/v1\.2\.3\/loom-vcs-v1\.2\.3-x86_64-pc-windows-msvc\.zip$/);

    const wingetDir = path.join(tmpDir, "packaging", "winget", "manifests", "w", "WickLim", "LoomVcs", "1.2.3");
    const verYaml = fs.readFileSync(path.join(wingetDir, "WickLim.LoomVcs.yaml"), "utf8");
    const instYaml = fs.readFileSync(path.join(wingetDir, "WickLim.LoomVcs.installer.yaml"), "utf8");
    const locYaml = fs.readFileSync(path.join(wingetDir, "WickLim.LoomVcs.locale.en-US.yaml"), "utf8");

    assert.match(verYaml, /PackageIdentifier: WickLim\.LoomVcs/);
    assert.match(verYaml, /PackageVersion: 1\.2\.3/);
    assert.match(instYaml, /RelativeFilePath: loom\.exe/);
    assert.match(instYaml, /PortableCommandAlias: loom/);
    assert.match(instYaml, new RegExp(`InstallerSha256: ${"5".repeat(64)}`));
    assert.match(locYaml, /ManifestType: defaultLocale/);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
});

test("fails loudly when SHA256SUMS.txt is missing a target archive", () => {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "loom-manifest-err-"));
  try {
    const checksumsPath = path.join(tmpDir, "SHA256SUMS.txt");
    fs.writeFileSync(
      checksumsPath,
      `${"a".repeat(64)}  loom-vcs-v0.1.0-x86_64-unknown-linux-gnu.tar.gz\n`,
      "utf8"
    );

    const res = childProcess.spawnSync(
      process.execPath,
      [generatorPath, "--version", "0.1.0", "--checksums", checksumsPath, "--out-root", tmpDir],
      { encoding: "utf8" }
    );
    assert.notEqual(res.status, 0);
    assert.match(res.stderr, /Missing SHA256 checksums for:/);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
});
