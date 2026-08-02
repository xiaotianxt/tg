## Direct encrypted database reader

tg now reads the encrypted source databases and live WAL directly. The retired
`refresh` and `decrypt` commands and `~/.tg/decrypted` cache are no longer used.

After verifying the new release, inspect and remove the exact legacy cache:

```bash
du -sh ~/.tg/decrypted
rm -rf ~/.tg/decrypted
```

Custom historical output directories are never guessed or deleted.
