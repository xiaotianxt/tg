class Tgreader < Formula
  desc "macOS Telegram 聊天记录读取 CLI 工具"
  homepage "https://github.com/xiaotianxt/tgreader"
  url "https://github.com/xiaotianxt/tgreader.git",
      tag:      "v0.1.0",
      revision: "HEAD"
  license "MIT"
  head "https://github.com/xiaotianxt/tgreader.git", branch: "main"

  depends_on "rust" => :build
  depends_on xcode: :build

  def install
    # 编译 C 内存扫描器
    system "cc", "-O2", "-o", "scanner_macos",
           "vendor/find_all_keys_macos.c",
           "-framework", "Foundation"

    # 编译 Rust CLI
    system "cargo", "install", "--bin", "tgreader", "--root", prefix, "."

    # 安装 scanner 到 bin 目录
    bin.install "scanner_macos"
  end

  test do
    system "#{bin}/tgreader", "--help"
  end
end
