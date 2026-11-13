## Images load when the MCP server runs as a service

When launchd or systemd starts `tg mcp serve`, the service gets a minimal PATH
without the Homebrew prefix. `get_media` then could not find `ffmpeg` and
failed for `tggf` images with "run ffmpeg for tggf: No such file or directory".
Exports run from a service lost the voice decoder the same way.

tg now looks for ffmpeg and the voice decoder on PATH, then in the Homebrew
prefix tg was installed into, then in the default Homebrew `bin` directories.
`TG_FFMPEG` and `TG_VOICE_DECODER` still take precedence. The Homebrew formula
now installs ffmpeg.

After upgrading, restart the service so it runs the new version.

## Failures are easier to tell apart

- `tg doctor` and the MCP `diagnose` tool show which ffmpeg and voice decoder
  tg will use, or that one is missing.
- Errors distinguish ffmpeg not being found, ffmpeg failing to start, and a
  failed decode.
- `get_media` reports a tool error when no image could be returned, instead of
  a normal result that only contains the error text.
