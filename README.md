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

## Supported platforms

**Support policy: latest LTS of each platform only.** Anything older is
best-effort and untested; CI builds and tests against the versions below.

| Platform | Supported | Distribution artifacts | Autostart | Notes |
|---|---|---|---|---|
| Linux x86_64 / aarch64 | Latest Ubuntu LTS (24.04) and equivalent-era distros (Debian stable, current Fedora, …) | AppImage (GUI, libs bundled), static musl tar.gz (headless CLI) | systemd `--user` unit | AppImage needs FUSE (or `--appimage-extract-and-run`); GUI also needs GTK3 + libayatana-appindicator, both bundled in the AppImage |
| macOS Apple Silicon / Intel | Latest macOS (15 Sequoia) | zip of the bare binary — no .dmg, no signing | launchd LaunchAgent | Binary is unsigned: if downloaded via a browser, run `xattr -d com.apple.quarantine guillotine` once |
| Windows 10/11 x64 | Latest Windows 10/11 | zip of `guillotine.exe` (static CRT — no redistributable needed) | registry Run key (default launch: daemon + tray) | GUI-first: double-clicking the exe starts the service and the tray; the exe is a windowed-subsystem binary, so CLI subcommands (`config init`, `scan`, …) produce no console output — use the tray app |

Feature coverage is identical on all platforms:

| Feature | Linux | macOS | Windows |
|---|---|---|---|
| Tray GUI | yes | yes | yes |
| Headless CLI (`service run`, `scan`, `archive`, `viewer`, …) | yes | yes | yes (no console output, see above) |
| Archive viewer (local web UI) | yes | yes | yes |
| Native service autostart | systemd | launchd | registry |
| V1-compatible archive format | yes | yes | yes |

Known limitations:

- No 32-bit, armv7, or Windows-on-ARM builds; Linux musl builds are
  headless-only (the GUI needs glibc + GTK).
- Config/secret files are `0600` and the data dir `0700` on unix; on Windows
  privacy relies on the user profile ACLs.
- The archive viewer has no authentication by design — it binds to loopback
  only unless `--expose` is passed.

## Quickstart

Everything lives in the crate at the repo root:

```bash
git clone https://github.com/bashout/message-guillotine.git
cd message-guillotine
cargo build --release            # add --no-default-features on headless Linux

# the binary is target/release/guillotine
# No subcommand (desktop builds) starts the service daemon and the tray
# app together; if a daemon is already running it is reused, not doubled.
# A first launch with no config yet writes the example config automatically.
./target/release/guillotine                # service + tray (desktop builds)
./target/release/guillotine config init      # writes an example config (0600) to the per-OS config dir
./target/release/guillotine config validate  # check it and print the resolved chats
# edit the config: token (or GUILLOTINE_TOKEN env var), [dm] policy, [chat.*] entries

./target/release/guillotine service install  # register native autostart
./target/release/guillotine service status
./target/release/guillotine gui              # tray app (desktop builds; ensures the daemon)
./target/release/guillotine run-now dm:USER_ID          # manual run for one chat (serialized with scheduled runs)
./target/release/guillotine scan CHANNEL_ID --retention-days 7   # dry-run report
./target/release/guillotine archive CHANNEL_ID              # one-shot archive
./target/release/guillotine viewer [ARCHIVES_DIR] --host 127.0.0.1 --port 8765   # browse archives (--no-browser to skip opening)
```

On Windows and macOS the autostart entries register the same default launch,
so login starts the daemon and the tray together (the Linux systemd unit
stays headless: `service run`). The daemon runs detached in its own process,
but exiting the tray app cleans up the service instance: Quit (and the
tray's "Stop service" item) request a clean shutdown over IPC — logs
flushed, IPC port file removed. Closing the panel window (X) only hides it.
With no working token yet the service still comes up (run-now works as soon
as a token is configured): it tries authentication three times per token
value and then idles, checking the config for a token update every few
seconds until a new value appears — then it tries three times again, and so
on.

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

All development happens in the crate at the repo root.

Toolchain prerequisites:

- **Rust** (stable, via [rustup](https://rustup.rs)) — the default toolchain is
  used on all platforms.
- **Linux only:** the GUI/tray crates link against GTK3 and libayatana-appindicator
  (`libxdo-dev libayatana-appindicator3-dev pkg-config libgtk-3-dev` on
  Ubuntu; the same set CI installs). Skip these for a headless
  (`--no-default-features`) build.
- **Windows only:** the MSVC linker from [Visual Studio Build
  Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio)
  ("Desktop development with C++", which includes the Windows SDK) — rustup's
  default `x86_64-pc-windows-msvc` target needs `link.exe` from it. No other
  system libraries: the tray/GUI crates are pure Rust on Windows. The `winget`
  equivalent is `Microsoft.VisualStudio.2022.BuildTools` (with the C++ and
  Windows SDK components) plus `Rustlang.Rustup`.
- **macOS:** Xcode Command Line Tools (`xcode-select --install`).

```bash
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
