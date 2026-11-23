#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_OUT_ROOT = path.resolve(__dirname, "..");

const ARCHIVE_SPECS = Object.freeze([
  {
    key: "darwin-arm64",
    target: "aarch64-apple-darwin",
    ext: "tar.gz",
  },
  {
    key: "darwin-x64",
    target: "x86_64-apple-darwin",
    ext: "tar.gz",
  },
  {
    key: "linux-x64",
    target: "x86_64-unknown-linux-gnu",
    ext: "tar.gz",
  },
  {
    key: "linux-arm64",
    target: "aarch64-unknown-linux-gnu",
    ext: "tar.gz",
  },
  {
    key: "win32-x64",
    target: "x86_64-pc-windows-msvc",
    ext: "zip",
  },
  {
    key: "win32-arm64",
    target: "aarch64-pc-windows-msvc",
    ext: "zip",
  },
]);

const NPM_PACKAGES = Object.freeze([
  "loom-vcs",
  "loom-vcs-darwin-arm64",
  "loom-vcs-darwin-x64",
  "loom-vcs-linux-x64",
  "loom-vcs-linux-arm64",
  "loom-vcs-win32-x64",
  "loom-vcs-win32-arm64",
]);

function parseArgs(argv) {
  const opts = {
    version: null,
    repo: "Wick-Lim/loom",
    checksums: null,
    outRoot: DEFAULT_OUT_ROOT,
    placeholder: false,
    syncNpm: false,
    syncNpmOnly: false,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    switch (arg) {
      case "--version":
        opts.version = argv[++i];
        break;
      case "--repo":
        opts.repo = argv[++i];
        break;
      case "--checksums":
        opts.checksums = argv[++i];
        break;
      case "--out-root":
        opts.outRoot = path.resolve(argv[++i]);
        break;
      case "--placeholder":
        opts.placeholder = true;
        break;
      case "--sync-npm":
        opts.syncNpm = true;
        break;
      case "--sync-npm-only":
        opts.syncNpmOnly = true;
        opts.syncNpm = true;
        break;
      default:
        throw new Error(`Unknown CLI argument: ${arg}`);
    }
  }

  if (!opts.version) {
    throw new Error("--version <X.Y.Z> is required");
  }
  const cleanVer = opts.version.replace(/^v/, "").trim();
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(cleanVer)) {
    throw new Error(`Invalid semver version "${opts.version}"`);
  }
  opts.version = cleanVer;

  if (!opts.syncNpmOnly && !opts.checksums && !opts.placeholder) {
    throw new Error("Either --checksums <PATH> or --placeholder must be provided");
  }

  return opts;
}

function archiveName(version, spec) {
  return `loom-vcs-v${version}-${spec.target}.${spec.ext}`;
}

function parseChecksums(checksumText, version) {
  const byFilename = new Map();
  for (const rawLine of checksumText.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    const match = /^([a-fA-F0-9]{64})\s+\*?(.+)$/.exec(line);
    if (!match) {
      throw new Error(`Unrecognized SHA256SUMS line: ${line}`);
    }
    const sha = match[1].toLowerCase();
    const rawFile = match[2].trim();
    const base = path.posix.basename(rawFile.replace(/\\/g, "/"));
    byFilename.set(base, sha);
  }

  const resolved = {};
  const missing = [];
  for (const spec of ARCHIVE_SPECS) {
    const filename = archiveName(version, spec);
    const sha = byFilename.get(filename);
    if (!sha) {
      missing.push(filename);
    } else {
      resolved[spec.key] = { filename, sha256: sha };
    }
  }

  if (missing.length > 0) {
    throw new Error(`Missing SHA256 checksums for: ${missing.join(", ")}`);
  }
  return resolved;
}

function placeholderChecksums(version) {
  const resolved = {};
  const zeroHash = "0".repeat(64);
  for (const spec of ARCHIVE_SPECS) {
    resolved[spec.key] = {
      filename: archiveName(version, spec),
      sha256: zeroHash,
    };
  }
  return resolved;
}

function writeUtf8Lf(filePath, content) {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  const normalized = content.replace(/\r\n/g, "\n").replace(/\n*$/, "\n");
  fs.writeFileSync(filePath, normalized, { encoding: "utf8" });
}

function renderHomebrewFormula(version, repo, sums) {
  const baseUrl = `https://github.com/${repo}/releases/download/v${version}`;
  return `class LoomVcs < Formula
  desc "Multi-agent native, local-first version control system (Project Loom)"
  homepage "https://github.com/${repo}"
  version "${version}"
  license "MIT OR Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "${baseUrl}/${sums["darwin-arm64"].filename}"
      sha256 "${sums["darwin-arm64"].sha256}"
    else
      url "${baseUrl}/${sums["darwin-x64"].filename}"
      sha256 "${sums["darwin-x64"].sha256}"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "${baseUrl}/${sums["linux-arm64"].filename}"
      sha256 "${sums["linux-arm64"].sha256}"
    else
      url "${baseUrl}/${sums["linux-x64"].filename}"
      sha256 "${sums["linux-x64"].sha256}"
    end
  end

  def install
    bin.install "loom"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/loom --version")
  end
end
`;
}

function renderScoopManifest(version, repo, sums) {
  const baseUrl = `https://github.com/${repo}/releases/download/v${version}`;
  const manifest = {
    version,
    description: "Multi-agent native, local-first version control system (Project Loom)",
    homepage: `https://github.com/${repo}`,
    license: "MIT OR Apache-2.0",
    architecture: {
      "64bit": {
        url: `${baseUrl}/${sums["win32-x64"].filename}`,
        hash: sums["win32-x64"].sha256,
      },
      arm64: {
        url: `${baseUrl}/${sums["win32-arm64"].filename}`,
        hash: sums["win32-arm64"].sha256,
      },
    },
    bin: "loom.exe",
    checkver: {
      github: `https://github.com/${repo}`,
    },
    autoupdate: {
      architecture: {
        "64bit": {
          url: `https://github.com/${repo}/releases/download/v$version/loom-vcs-v$version-x86_64-pc-windows-msvc.zip`,
        },
        arm64: {
          url: `https://github.com/${repo}/releases/download/v$version/loom-vcs-v$version-aarch64-pc-windows-msvc.zip`,
        },
      },
      hash: {
        url: "$baseurl/SHA256SUMS.txt",
      },
    },
  };
  return `${JSON.stringify(manifest, null, 2)}\n`;
}

function renderWingetFiles(version, repo, sums) {
  const baseUrl = `https://github.com/${repo}/releases/download/v${version}`;
  const versionYaml = `# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json
PackageIdentifier: WickLim.LoomVcs
PackageVersion: ${version}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
`;

  const installerYaml = `# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json
PackageIdentifier: WickLim.LoomVcs
PackageVersion: ${version}
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
  - RelativeFilePath: loom.exe
    PortableCommandAlias: loom
Installers:
  - Architecture: x64
    InstallerUrl: ${baseUrl}/${sums["win32-x64"].filename}
    InstallerSha256: ${sums["win32-x64"].sha256.toUpperCase()}
  - Architecture: arm64
    InstallerUrl: ${baseUrl}/${sums["win32-arm64"].filename}
    InstallerSha256: ${sums["win32-arm64"].sha256.toUpperCase()}
ManifestType: installer
ManifestVersion: 1.6.0
`;

  const localeYaml = `# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.1.6.0.schema.json
PackageIdentifier: WickLim.LoomVcs
PackageVersion: ${version}
PackageLocale: en-US
Publisher: Wick-Lim
PublisherUrl: https://github.com/Wick-Lim
PublisherSupportUrl: https://github.com/${repo}/issues
PackageName: Project Loom
PackageUrl: https://github.com/${repo}
License: MIT OR Apache-2.0
LicenseUrl: https://github.com/${repo}/blob/main/LICENSE-MIT
ShortDescription: Multi-agent native, local-first version control system (loom CLI + loomd daemon)
Moniker: loom
Tags:
  - vcs
  - git
  - ai-agents
  - mcp
  - tree-sitter
ReleaseNotesUrl: https://github.com/${repo}/releases/tag/v${version}
ManifestType: defaultLocale
ManifestVersion: 1.6.0
`;

  return { versionYaml, installerYaml, localeYaml };
}

function syncNpmPackages(outRoot, version, strict) {
  const npmRoot = path.join(outRoot, "npm");
  if (!fs.existsSync(npmRoot)) {
    if (strict) {
      throw new Error(`npm directory not found at ${npmRoot}`);
    }
    return [];
  }

  const updated = [];
  for (const pkg of NPM_PACKAGES) {
    const pkgPath = path.join(npmRoot, pkg, "package.json");
    if (!fs.existsSync(pkgPath)) {
      if (strict) {
        throw new Error(`Missing package.json: ${pkgPath}`);
      }
      continue;
    }
    const parsed = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
    parsed.version = version;
    if (parsed.optionalDependencies) {
      for (const depName of Object.keys(parsed.optionalDependencies)) {
        parsed.optionalDependencies[depName] = version;
      }
    }
    writeUtf8Lf(pkgPath, `${JSON.stringify(parsed, null, 2)}\n`);
    updated.push(pkgPath);
  }
  return updated;
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  const written = [];

  if (!opts.syncNpmOnly) {
    const sums = opts.checksums
      ? parseChecksums(fs.readFileSync(opts.checksums, "utf8"), opts.version)
      : placeholderChecksums(opts.version);

    const formulaPath = path.join(opts.outRoot, "Formula", "loom-vcs.rb");
    writeUtf8Lf(formulaPath, renderHomebrewFormula(opts.version, opts.repo, sums));
    written.push(formulaPath);

    const scoopPath = path.join(opts.outRoot, "packaging", "scoop", "loom-vcs.json");
    writeUtf8Lf(scoopPath, renderScoopManifest(opts.version, opts.repo, sums));
    written.push(scoopPath);

    const wingetDir = path.join(
      opts.outRoot,
      "packaging",
      "winget",
      "manifests",
      "w",
      "WickLim",
      "LoomVcs",
      opts.version
    );
    const winget = renderWingetFiles(opts.version, opts.repo, sums);
    const wingetVerPath = path.join(wingetDir, "WickLim.LoomVcs.yaml");
    const wingetInstPath = path.join(wingetDir, "WickLim.LoomVcs.installer.yaml");
    const wingetLocalePath = path.join(wingetDir, "WickLim.LoomVcs.locale.en-US.yaml");

    writeUtf8Lf(wingetVerPath, winget.versionYaml);
    writeUtf8Lf(wingetInstPath, winget.installerYaml);
    writeUtf8Lf(wingetLocalePath, winget.localeYaml);
    written.push(wingetVerPath, wingetInstPath, wingetLocalePath);
  }

  if (opts.syncNpm || fs.existsSync(path.join(opts.outRoot, "npm"))) {
    written.push(...syncNpmPackages(opts.outRoot, opts.version, opts.syncNpm));
  }

  process.stdout.write(`Generated/updated ${written.length} files for v${opts.version}\n`);
}

main();
