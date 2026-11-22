## Linux key capture uses a kernel uprobe

`tg keys --method login` on Linux no longer attaches gdb to the desktop client.
It registers a kernel uprobe at the account-material hook and reads the material
from the probe hit, then removes the probe. The command, flags, messages and
output file are unchanged.

- gdb is no longer required on Linux; `sudo apt install gdb` is no longer part
  of the setup.
- Capture needs root (CAP_SYS_ADMIN) and a mounted tracefs with
  `CONFIG_UPROBE_EVENTS` (`/sys/kernel/tracing` or `/sys/kernel/debug/tracing`).
  A missing mount or permission is reported with the mount command to run.

## Consistent app name in tg's own messages

The installed skill and the MCP tool descriptions already named the desktop
app by its real name, but a few messages tg prints itself still used the
placeholder name. These now use the same name:

- `tg --help`
- the process line in `tg doctor`
- the error from `tg keys --method login` when the app is not running
- the export warning when the app's data directory is missing
- the first line of an exported `chat.txt`

Message text is never rewritten.
