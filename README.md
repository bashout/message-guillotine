# Message Guillotine

![Message Guillotine](branding/message_guillotine_banner.jpg)

Bulk delete and archive your DM and server messages via the platform's unofficial user API (self-bot — violates the platform ToS, use at your own risk).

## Features

| Feature | Description |
|---------|-------------|
| DM deletion | Delete your own DM messages, filtered by user and time range; the channel is scanned newest-first and each of your messages is deleted as it is found, with live progress and a remaining-time estimate |
| Server deletion | Delete your own messages from a server channel, filtered by time range, with the same streaming scan |
| Rate limiting | 24 messages/minute; the server's rate-limit headers are honored when the bucket is exhausted |
| System messages | Calls, pins and other system messages are filtered out before any delete is attempted — a stack of them never burns rate-limit budget |
| Archiving | Save a full chat — both sides, oldest first — as `messages.jsonl` + `attachments/`, streamed page by page |
| Resume | Interrupted archives and deletion runs pick up where they stopped: archives via per-channel cursors, deletions via a resumable per-channel session (one keypress, no rescanning); partial archive writes are cleaned up automatically |
| Dry run | `guillotine.py --dry-run` reports what would be deleted (count + message samples) without deleting |
| Time filters | Absolute dates (`YYYY-MM-DD [HH:MM[:SS]]`) or relative: `7d`, `2w`, `today`, `yesterday` |
| Merging | Archiving a chat that has no archive of its own can be merged into an existing archive folder; both channels then resume and view as one archive |
| Attachments | Downloaded with atomic writes and retried on resume; runs that end early are flagged incomplete in `status.json` |
| Viewer | Local web UI to browse archives: search, date/attachment/link filters, reply threading, media gallery, stats — launched from menu option `[5]`, `archive-viewer`, or the CLI, opens your browser automatically (`archive-viewer --no-browser` to skip) |
| Config | Archive folder set via `guillotine.py --archive-dir FOLDER`, `DM_ARCHIVE_DIR`, or menu option `[4]`; per-OS default otherwise |

## Development

Requires Python 3.8+. `requests` is the only runtime dependency.

```bash
git clone https://github.com/bashout/message-guillotine.git
cd message-guillotine

# Run with uv (pinned deps from uv.lock)
uv run guillotine.py                # deleter / archiver (--archive-dir FOLDER to set the archival folder)
uv run archive_viewer.py            # archive viewer (localhost web UI; --host/--port/--no-browser)

# Or install
pip install .
message-guillotine
archive-viewer
```

The tool prompts for your user token on start. Archives are written to your
archive folder (one folder per chat); the viewer serves that folder.

Run the tests (config resolution, archive format, archiver resume/retry,
API paging, deletion streaming/resume, viewer queries, and the HTTP server
end to end):

```bash
uv sync --extra dev --locked
uv run pytest
```

Build a standalone binary (PyInstaller; includes the viewer):

```bash
uv sync --extra build --locked
uv run python build_exe.py
```
