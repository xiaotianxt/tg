# tgreader

A CLI tool to read Telegram messages from local encrypted databases on macOS.

## Requirements

- macOS (tested on Telegram 4.x)
- [Rust](https://rustup.rs/)
- C compiler (for the memory scanner)
- SIP partially disabled or Telegram re-signed (for `task_for_pid`)

## Usage

```bash
# 1. Extract encryption keys from Telegram process memory
make scanner
sudo ./scanner_macos

# 2. Decrypt all databases
cargo run -- decrypt

# 3. List chat sessions
cargo run -- sessions

# 4. Read messages
cargo run -- messages "Contact Name"

# 5. Search across all sessions
cargo run -- search "keyword"

# 6. Export to file
cargo run -- export "Contact Name" --format txt
```

## Subcommands

| Command     | Description                                      |
|-------------|--------------------------------------------------|
| `keys`      | Extract DB encryption keys (requires sudo)       |
| `decrypt`   | Decrypt all SQLCipher-encrypted databases        |
| `sessions`  | List chat sessions sorted by message count       |
| `messages`  | Read messages from a specific session            |
| `search`    | Search across all sessions                       |
| `export`    | Export chat to TXT/CSV/JSON                      |

## How it works

Telegram on macOS stores chat data in SQLCipher 4 encrypted SQLite databases. The encryption key is stored in Telegram's process memory. tgreader:

1. Scans Telegram's memory via the Mach VM API (`task_for_pid` + `mach_vm_read`) to find keys in WCDB's format
2. Cross-references keys with database file salts
3. Decrypts databases using AES-256-CBC with HMAC-SHA512 verification
4. Reads contact info and messages from the decrypted databases

## Project structure

```
tgreader/
├── src/
│   ├── main.rs       # CLI entry point (clap)
│   ├── scanner.rs    # Key extraction via subprocess
│   ├── decrypt.rs    # SQLCipher 4 decryption
│   ├── db.rs         # Database reading and querying
│   └── export.rs     # Message export (TXT/CSV/JSON)
├── vendor/
│   └── find_all_keys_macos.c  # Mach VM memory scanner
└── Cargo.toml
```

## License

MIT
