## Group members and local friend lookup

Patch follow-up: update export error handling and fixed-size block iteration for
current Clippy, and require successful CI for the exact release commit before
the release script publishes a tag.

- Add `tg members GROUP --anonymous` to list the cached group roster with locally recorded speaking activity.
- Add `--inactive-since` to identify members with no recorded activity since a cutoff, with explicit local-history coverage warnings.
- Add `tg members GROUP --friends --anonymous` to find locally confirmed friends in the cached roster without reading message history, including newly joined groups.
- Friend results use conservative local relationship evidence, not a live or reciprocal friendship check.
- Restore the bundled skill installer.

All source database access remains local and read-only. No messages are sent and no group membership is changed.
