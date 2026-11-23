class LoomVcs < Formula
  desc "Multi-agent native, local-first version control system (Project Loom)"
  homepage "https://github.com/Wick-Lim/loom"
  version "0.1.0"
  license "MIT OR Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Wick-Lim/loom/releases/download/v0.1.0/loom-vcs-v0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/Wick-Lim/loom/releases/download/v0.1.0/loom-vcs-v0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Wick-Lim/loom/releases/download/v0.1.0/loom-vcs-v0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/Wick-Lim/loom/releases/download/v0.1.0/loom-vcs-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "loom"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/loom --version")
  end
end
