class Tgreader < Formula
  desc "macOS Telegram 聊天记录读取 CLI 工具"
  homepage "https://github.com/xiaotianxt/tgreader"
  license "MIT"

  depends_on "rust" => :build

  head do
    url "https://github.com/xiaotianxt/tgreader.git", branch: "main"
  end

  stable do
    url "https://github.com/xiaotianxt/tgreader.git",
        tag:      "v0.1.0",
        revision: "ac27f67"
  end

  def install
    system "cc", "-O2", "-o", "scanner_macos",
           "vendor/find_all_keys_macos.c",
           "-framework", "Foundation"
    system "cargo", "install", "--bin", "tgreader", "--root", prefix, "."
    bin.install "scanner_macos"
  end

  test do
    system "#{bin}/tgreader", "--help"
  end
end
