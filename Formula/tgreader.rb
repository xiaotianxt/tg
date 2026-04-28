class Tgreader < Formula
  desc "macOS Telegram 聊天记录读取 CLI 工具"
  homepage "https://github.com/xiaotianxt/tgreader"
  url "https://github.com/xiaotianxt/tgreader.git",
      tag:      "v0.1.0",
      revision: "e75d2c0"
  license "MIT"
  head "https://github.com/xiaotianxt/tgreader.git", branch: "main"

  depends_on "rust" => :build
  depends_on xcode: :build

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
