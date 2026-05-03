#!/bin/bash
set -euo pipefail

echo "==> tg 安装脚本"
echo ""

# --- 检测 Rust ---
if ! command -v cargo &>/dev/null; then
    echo "错误: 未找到 Rust 工具链。请先安装: https://rustup.rs/"
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
echo "==> 编译 Rust CLI..."
cargo build --release

# --- 安装 ---
echo ""
echo "==> 安装二进制文件..."
if [[ "$BIN_DIR" == "/usr/local/bin" ]]; then
    sudo cp target/release/tg "$BIN_DIR/"
else
    cp target/release/tg "$BIN_DIR/"
fi

# --- 配置 Claude Code skill ---
echo ""
echo "==> 配置 Claude Code skill..."
CLAUDE_SETTINGS_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
SKILL_DIR="$CLAUDE_SETTINGS_DIR/skills/tg"
target/release/tg skill install --dir "$SKILL_DIR"
echo "已安装 Claude Code skill 到 $SKILL_DIR/SKILL.md"

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
