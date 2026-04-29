#!/bin/bash
set -euo pipefail

echo "==> tg 安装脚本"
echo ""

# --- 检测 Rust ---
if ! command -v cargo &>/dev/null; then
    echo "错误: 未找到 Rust 工具链。请先安装: https://rustup.rs/"
    exit 1
fi

# --- 检测 cc ---
if ! command -v cc &>/dev/null; then
    echo "错误: 未找到 C 编译器。请安装 Xcode Command Line Tools:"
    echo "  xcode-select --install"
    exit 1
fi

# --- 确定安装目录 ---
if [[ "${1:-}" == "--local" ]]; then
    BIN_DIR="$HOME/.local/bin"
    mkdir -p "$BIN_DIR"
    echo "安装到用户目录: $BIN_DIR"
    echo "请确保 $BIN_DIR 在 PATH 中"
else
    BIN_DIR="/usr/local/bin"
    echo "安装到系统目录: $BIN_DIR"
    echo "可能需要输入密码..."
fi

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR"

# --- 编译 ---
echo ""
echo "==> 编译 C 内存扫描器..."
cc -O2 -o scanner_macos vendor/find_all_keys_macos.c -framework Foundation

echo "==> 编译 Rust CLI..."
cargo build --release

# --- 安装 ---
echo ""
echo "==> 安装二进制文件..."
if [[ "$BIN_DIR" == "/usr/local/bin" ]]; then
    sudo cp scanner_macos "$BIN_DIR/"
    sudo cp target/release/tg "$BIN_DIR/"
else
    cp scanner_macos "$BIN_DIR/"
    cp target/release/tg "$BIN_DIR/"
fi

# --- 配置 Claude Code 技能 ---
echo ""
echo "==> 配置 Claude Code 技能..."
CLAUDE_SETTINGS_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
mkdir -p "$CLAUDE_SETTINGS_DIR"

# 合并 settings.json（如果已存在）
SKILL_CONFIG='{
  "skills": {
    "tg": {
      "name": "tg",
      "prompt": "Telegram聊天记录读取工具 tg 已安装。

可用命令：
  tg keys              # 提取密钥（需 sudo）
  tg decrypt            # 解密数据库
  tg sessions           # 列出会话
  tg messages <会话名>   # 读取消息（支持 --limit --offset --search）
  tg search <关键词>     # 全文搜索
  tg export <会话名>     # 导出（--format txt|csv|json）

工作流程：
1. 确保Telegram正在运行
2. sudo tg keys → 提取密钥到 all_keys.json
3. tg decrypt → 解密到 decrypted/
4. tg sessions → 查看会话列表
5. tg messages \"联系人\" → 读消息"
    }
  }
}'

if [[ -f "$CLAUDE_SETTINGS_DIR/settings.json" ]]; then
    # 使用 python3 合并 JSON（更安全）
    python3 -c "
import json
import sys

with open('$CLAUDE_SETTINGS_DIR/settings.json') as f:
    existing = json.load(f)

with open('/dev/stdin') as f:
    new = json.load(f)

# Merge skills
skills = existing.get('skills', {})
skills.update(new.get('skills', {}))
existing['skills'] = skills

with open('$CLAUDE_SETTINGS_DIR/settings.json', 'w') as f:
    json.dump(existing, f, ensure_ascii=False, indent=2)
" <<< "$SKILL_CONFIG"
    echo "已合并到 $CLAUDE_SETTINGS_DIR/settings.json"
else
    echo "$SKILL_CONFIG" > "$CLAUDE_SETTINGS_DIR/settings.json"
    echo "已创建 $CLAUDE_SETTINGS_DIR/settings.json"
fi

# --- 完成 ---
echo ""
echo "========================================"
echo "  tg 安装完成！"
echo "========================================"
echo ""
echo "快速开始："
echo "  1. 确保Telegram正在运行"
echo "  2. sudo tg keys        # 提取密钥"
echo "  3. tg decrypt          # 解密数据库"
echo "  4. tg sessions         # 查看会话"
echo "  5. tg messages \"名称\"  # 读取消息"
echo ""
echo "在 Claude Code 中可使用 /tg 加载技能。"
