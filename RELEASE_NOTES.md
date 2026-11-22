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
