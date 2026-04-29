# tg 项目指南

## 项目简介

tg 是一款 macOS Telegram聊天记录读取 CLI 工具。它从Telegram进程内存中提取 SQLCipher 4 密钥，解密本地数据库，然后提供命令行查询和导出功能。

## 快速命令

```bash
make install                 # 编译 + 安装到 /usr/local/bin
sudo tg keys           # 从内存提取密钥
tg decrypt             # 默认静默增量解密数据库
tg decrypt --full      # 强制全量重解
tg sessions            # 列出会话
tg messages "名称"     # 读取消息
tg search "关键词"     # 全文搜索
tg export "名称" -f txt # 导出
```

## 架构

| 模块 | 职责 | 关键技术 |
|------|------|----------|
| `src/main.rs` | CLI 入口 | clap 子命令解析 |
| `src/scanner.rs` | 密钥提取 | 调用 `scanner_macos` 子进程 |
| `src/decrypt.rs` | 数据库解密 | AES-256-CBC, HMAC-SHA512, PBKDF2 |
| `src/db.rs` | 数据查询 | rusqlite, 联系人/消息读取 |
| `src/export.rs` | 消息导出 | TXT/CSV/JSON |
| `vendor/find_all_keys_macos.c` | 内存扫描 | Mach VM API |

## 关键数据流

1. **密钥提取**: `sudo tg keys` → C 扫描器遍历Telegram内存 → 查找 `x'<96hex>'` 模式 → 输出 `all_keys.json`
2. **数据库解密**: `tg decrypt` → 读取 `all_keys.json` → 默认静默增量刷新 → 逐页 AES-256-CBC 解密变化内容 → 验证 SQLite → 输出到 `decrypted/`
3. **消息查询**: `tg messages` → 先静默刷新 `decrypted/` 缓存 → 打开解密后的 `message/message_0.db` → 通过 MD5(username) 定位 `Msg_<hash>` 表 → 解析 tgid 前缀为联系人昵称 → 展示

## 数据库结构

- **contact/contact.db**: `contact` 表 (username, nick_name, remark, alias)
- **message/message_0.db**: `Msg_<md5(username)>` 表 (local_type, create_time, message_content, WCDB_CT_message_content)
- **session/session.db**: `SessionTable` 会话列表
- **表名**: `Msg_` + MD5(用户名/群 ID)

## 消息类型

| type | 含义 |
|------|------|
| 1 | 文本 |
| 3 | 图片 |
| 34 | 语音 |
| 43 | 视频 |
| 47 | 表情 |
| 49 | 链接/文件/小程序 |
| 10000 | 系统提示 |
| 10002 | 撤回消息 |

## 注意事项

- 群聊消息 content 格式: `tgid_xxx:\n消息内容`（冒号后换行）
- `message_content` 可能是 TEXT 或 BLOB（BLOB 用 `String::from_utf8` 兜底）
- `WCDB_CT_message_content` 为 4 表示压缩内容
- 解密后的数据库可能有索引损坏，用 `.recover` 恢复

## AI 辅助规则

- **修改密钥提取流程**时，确保 C 扫描器兼容 Telegram 4.x 的内存布局
- **修改解密逻辑**时，必须保持 HMAC 验证，否则可能输出损坏的 SQLite
- **修改消息读取**时，处理 TEXT/BLOB 双类型和 tgid 解析
- 所有错误信息应清晰可读，便于 AI 诊断
- 保持命令输出结构化（时间 发送者 内容），便于 AI 解析
