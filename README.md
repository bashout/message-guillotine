# Message Guillotine (V2 — Rust)

![Message Guillotine](branding/message_guillotine_banner.jpg)

Scheduled, service-style bulk delete and archive of your own chat messages.
This is the Rust V2 rewrite: configure your chats once, then the tool runs on
a schedule as a background service instead of driving an interactive menu.

WARNING: self-bots violate the platform's Terms of Service. Use at your own
risk.

## Features

| Feature | Description |
|---------|-------------|
| Per-chat schedules | Each chat (DM user or guild channel) gets its own `daily` / `weekly` / `monthly` schedule (optional `at HH:MM`) |
| Retention window | `retention_days` — the most recent N days of your messages are kept; only older messages are deleted |
| Archive before delete | Optional `archive_before_delete` flag: the chat is archived (appending to its existing archive) before deletion runs |
| Single-deletion lock | Only one deletion runs at a time, ever — a process-wide lock, enforced across processes via loopback IPC between the tray/CLI and the daemon |
| Tray GUI | Desktop tray app with status, run-now, settings, and logs |
| Viewer | `guillotine viewer` serves a local web UI to browse archives (search, filters, threading, media gallery, stats) |
| Service autostart | Native autostart: systemd `--user` on Linux, launchd LaunchAgent on macOS, registry Run key on Windows |
| Rate limiting | Paced at ~24 deletes/min; the platform's 429 `Retry-After` is honored |
| Resume | Interrupted deletion runs resume from per-channel session files; archives resume via per-channel cursors |
| Dry run | `guillotine scan CHANNEL_ID --retention-days 7` reports what a deletion run would delete, without deleting |
| DM scope | Nominate or exclude users only — there is no "all DMs" mode; scheduled deletion only touches configured chats |

Archives are written in the V1-compatible on-disk format (`meta.json`,
`messages.jsonl`, `manifest.jsonl`, `attachments/`, `status.json`), so old
V1 archives remain fully readable.

## Quickstart

Everything lives in [`rust/`](rust/):

```bash
git clone https://github.com/bashout/message-guillotine.git
cd message-guillotine/rust
cargo build --release            # add --no-default-features on headless Linux

# the binary is target/release/guillotine
./target/release/guillotine config init      # writes an example config (0600) to the per-OS config dir
./target/release/guillotine config validate  # check it and print the resolved chats
# edit the config: token (or GUILLOTINE_TOKEN env var), [dm] policy, [chat.*] entries

./target/release/guillotine service install  # register native autostart
./target/release/guillotine service status
./target/release/guillotine gui              # tray app (desktop builds)
./target/release/guillotine run-now dm:USER_ID          # manual run for one chat (serialized with scheduled runs)
./target/release/guillotine scan CHANNEL_ID --retention-days 7   # dry-run report
./target/release/guillotine archive CHANNEL_ID              # one-shot archive
./target/release/guillotine viewer [ARCHIVES_DIR] --host 127.0.0.1 --port 8765   # browse archives (--no-browser to skip opening)
```

## Configuration

Config file lives in the per-OS config dir as `config.toml` (override with
`--config` or the `GUILLOTINE_CONFIG` env var):

```toml
token = "..."                    # or GUILLOTINE_TOKEN env var
[dm]
policy = "exclude"               # "nominate" | "exclude" — no "all" mode
users = ["USER_ID"]
default_schedule = "weekly"
default_retention_days = 30
default_archive_before_delete = true

[chat."dm:333"]
schedule = "monthly"             # delete monthly, retain 7 days
retention_days = 7
archive_before_delete = true

[chat."guild:444/channel:555"]
schedule = "daily"
retention_days = 30
archive_before_delete = false
```

## Development

All development happens in [`rust/`](rust/):

```bash
cd rust
cargo test --release             # unit tests — must pass
cargo clippy --all-targets       # must be 0 warnings
cargo build --no-default-features  # headless build (no tray deps) must compile
```

See [AGENTS.md](AGENTS.md) for the hard rules (token handling, the
single-deletion lock, fail-safe deletion, atomic writes, loopback-only IPC,
the frozen archive format) and code conventions.

## Versioning

The V2 line is cut from git tags `v2.x.y` on `main`; the crate version is set
from that tag at release time.
