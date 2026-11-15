## View one image with `tg media`

Images show up in messages as `[img:<id>]`. Since `tg image` was removed, the
CLI could only decrypt images by exporting the whole chat, and the exported
files could not be traced back to their messages. Agents reading a chat
concluded that tg could not decrypt images at all.

`tg media CHAT ID...` decrypts just the images you name and prints one line per
id: which cached copy it used (`original`, `standard`, `thumbnail`, or a video
cover), the size, and the file path.

```bash
tg media "产品讨论群" e7c6bdd4fe5f994cd379e443ebc402b6
```

- Files go to `$TMPDIR/tg-media/<id>.<ext>` (directory 0700, files 0600);
  `--output DIR` writes elsewhere.
- A pasted `[img:<id>]` tag is accepted as well as the bare id.
- When only a thumbnail is cached, the output says so. An id with nothing
  cached is reported as `unavailable` and the command exits non-zero; open
  the image in Telegram first, then retry.

## Exported images are named by id

`tg export` now writes images and videos as `media/<id>.<ext>`, so an
`[img:<id>]` in `chat.txt` leads straight to its file. An image that appears
more than once is exported once. Files, stickers, and voices keep their
previous names. Scripts that relied on the old `<chat>_Image_3_NNNN.jpg` names
need updating.

## Read commands point to `tg media`

When `messages`, `search`, or table-format `query` output contains image
placeholders, it ends with one line showing the `tg media` command to use.
JSON output is unchanged.

After upgrading, run `tg skill install` so the installed agent skill describes
`tg media`.
