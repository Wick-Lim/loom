#!/bin/sh
set -eu

LOOM_REPO="${LOOM_REPO:-Wick-Lim/loom}"
LOOM_INSTALL_DIR="${LOOM_INSTALL_DIR:-$HOME/.local/bin}"
LOOM_VERSION="${LOOM_VERSION:-}"

say() {
  printf '%s\n' "$*"
}

err() {
  printf 'loom-vcs installer error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

fetch() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    err "curl or wget is required to download release artifacts"
  fi
}

fetch_stdout() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$url"
  else
    err "curl or wget is required to query GitHub releases"
  fi
}

verify_sha256() {
  expected="$1"
  file="$2"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  else
    err "sha256sum or shasum is required to verify download checksum"
  fi
  [ "$actual" = "$expected" ] || err "SHA256 checksum mismatch for $file (expected $expected, got $actual)"
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *) err "unsupported OS: $os (use PowerShell installer on Windows)" ;;
  esac

  case "$arch" in
    arm64 | aarch64) arch_part="aarch64" ;;
    x86_64 | amd64) arch_part="x86_64" ;;
    *) err "unsupported architecture: $arch" ;;
  esac

  printf '%s-%s' "$arch_part" "$os_part"
}

main() {
  need_cmd tar
  need_cmd uname
  need_cmd mktemp

  target="$(detect_target)"

  if [ -z "$LOOM_VERSION" ]; then
    say "Resolving latest loom-vcs release from https://github.com/$LOOM_REPO..."
    api_url="https://api.github.com/repos/$LOOM_REPO/releases/latest"
    tag="$(fetch_stdout "$api_url" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    [ -n "$tag" ] || err "failed to resolve latest release tag from $api_url"
  else
    case "$LOOM_VERSION" in
      v*) tag="$LOOM_VERSION" ;;
      *) tag="v$LOOM_VERSION" ;;
    esac
  fi

  archive="loom-vcs-${tag}-${target}.tar.gz"
  base_url="https://github.com/$LOOM_REPO/releases/download/$tag"

  tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t 'loom-install')"
  trap 'rm -rf "$tmp_dir"' EXIT INT TERM

  say "Downloading $archive..."
  fetch "$base_url/$archive" "$tmp_dir/$archive"
  fetch "$base_url/SHA256SUMS.txt" "$tmp_dir/SHA256SUMS.txt"

  expected_sha="$(awk -v f="$archive" '$2 == f || $2 == ("*" f) || $2 == ("./" f) { print $1; exit }' "$tmp_dir/SHA256SUMS.txt")"
  [ -n "$expected_sha" ] || err "checksum entry for $archive not found in SHA256SUMS.txt"
  verify_sha256 "$expected_sha" "$tmp_dir/$archive"

  tar -xzf "$tmp_dir/$archive" -C "$tmp_dir"
  [ -f "$tmp_dir/loom" ] || err "extracted archive did not contain 'loom' binary"

  mkdir -p "$LOOM_INSTALL_DIR"
  install_path="$LOOM_INSTALL_DIR/loom"
  tmp_bin="$install_path.tmp.$$"
  cp "$tmp_dir/loom" "$tmp_bin"
  chmod 0755 "$tmp_bin"
  mv -f "$tmp_bin" "$install_path"

  say "Installed loom-vcs ($tag) to $install_path"
  "$install_path" --version || true

  case ":${PATH:-}:" in
    *":$LOOM_INSTALL_DIR:"*) ;;
    *)
      say ""
      say "Add $LOOM_INSTALL_DIR to your PATH to run 'loom' from anywhere:"
      say "  export PATH=\"$LOOM_INSTALL_DIR:\$PATH\""
      ;;
  esac
}

main "$@"
