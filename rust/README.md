# guillotine (Rust, V2)

Scheduled, service-style bulk delete and archive of your own chat messages.
This is the Rust rewrite of the Python V1: same features, but configured once
and then left to run on a schedule instead of driving an interactive menu.

WARNING: self-bots violate the platform ToS. Use at your own risk.

## What it does

- Each chat (DM user or guild channel) has its own entry with a schedule
  (`daily` / `weekly` / `monthly`, optional `at HH:MM`), a retention window
  (`retention_days` — the most recent N days of your messages are kept), and
  an `archive_before_delete` flag.
- On each run (scheduled or "run now"), the chat is optionally archived first
  — always appending to the existing archive for that chat when one is
  present, in the V1-compatible on-disk format (`meta.json`, `messages.jsonl`,
  `manifest.jsonl`, `attachments/`, `status.json`) — then every message of
  yours older than the retention window is deleted.
- Only one deletion runs at a time (process-wide lock; the tray/CLI `run-now`
  goes through the daemon over loopback IPC so the guarantee holds across
  processes), paced at ~24 deletes/min with the platform's 429 `Retry-After`
  honored. Interrupted runs resume from per-channel session files.
- DM scope is nominated users or an exclusion list — there is no "all DMs"
  mode. Scheduled deletion only ever touches nominated guilds/channels.
- Runs as a service with native autostart (systemd --user on Linux, launchd
  LaunchAgent on macOS, registry Run key on Windows), plus a tray icon app
  (status, run-now, settings window) on desktop builds.

## Usage

```bash
cargo build --release            # add --no-default-features on headless Linux
guillotine config init           # writes an example config (0600) to the per-OS config dir
guillotine config validate       # check it and print the resolved chats
# edit the config: token (or GUILLOTINE_TOKEN), [dm] policy, [chat.*] entries
guillotine service install       # register native autostart
guillotine service status
guillotine gui                   # tray app (desktop builds)
guillotine run-now dm:USER_ID    # manual run for one chat (serialized with scheduled runs)
guillotine scan CHANNEL_ID --retention-days 7   # dry-run report
guillotine archive CHANNEL_ID    # one-shot archive
```

Config file (default per-OS config dir, override with `--config` or
`GUILLOTINE_CONFIG`):

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

The Python archive viewer from V1 still works against archives written by
this tool.

## Development

```bash
cargo test                       # unit tests
cargo build --no-default-features  # headless (no tray deps)
```

Release: the V2 line is cut as git tag `v2.x.y` when this branch merges to
main; the crate version is set from that tag at release time.
