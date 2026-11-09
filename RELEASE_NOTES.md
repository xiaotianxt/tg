## Bulk reads no longer starve the desktop client's database writes

A read on the live database keeps the desktop client from finishing its WAL
checkpoints. Running many tg commands back to back, such as a script that
dumps the whole history in time windows, held that read on the main message
database almost without a break for minutes, and the client dropped the
messages that arrived during that window.

On macOS, tg now holds the live database only long enough to make an APFS
copy-on-write clone of it and its WAL, then reads the clone for the rest of
the command. The clone stays encrypted, takes almost no disk space, and is
deleted when the command exits. Results are unchanged; every connection to one
database in a command now also reads the same snapshot.

Where cloning is unavailable (Linux, a non-APFS volume, or a `--db-dir` on a
different volume than `$TMPDIR`), tg reads the live database as before.
