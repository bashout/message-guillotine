# AGENTS.md — guillotine

Bulk delete/archive of your own Discord messages (self-bot; violates the platform ToS). The Rust service in this repo (branch `v2/rust-service`; released as git tag `v2.x.y` on merge to `main`). The old Python V1 was removed; archives remain format-compatible with V1 archives.

## Hard rules

- **Never log or print the token.** `Config`'s `Debug` impl redacts it; token only goes into the `Authorization` header via `HeaderValue::from_str`. Config/secret/session files are `0600`; the data dir is `0700`.
- **One deletion at a time, ever.** The process-wide `deletion_lock` covers archive+delete; tray/CLI run-now goes through the daemon's IPC so the lock holds across processes. Pace stays 24/min (2.6 s) with 429 `Retry-After` honored — do not raise it.
- **Fail safe on deletion.** An undatable message timestamp is skipped+counted, never treated as old. 404 after a network error counts as deleted; other 404s count failed.
- **Atomic writes for all state files** (sessions, meta/manifest/messages/status, config, last_runs): temp file + rename. Never write the config non-atomically.
- **IPC is loopback-only, secret-authenticated** (0600 secret in the data dir), line-capped, and every exchange is timeout-bounded. `run_now` acks immediately and runs in the background — never block the IPC reply on a deletion.
- **Archive format is frozen** (V1-compatible: `meta.json`, `messages.jsonl`, `manifest.jsonl`, `attachments/`, `status.json`). Appends/resume only; the Python and Rust viewers both read it.
- **No unconditional "all DMs" scope** — DM policy is nominate-or-exclude only. Scheduled deletion touches only configured chats.

## Conventions

- Rust: `cargo clippy` must be 0 warnings; `cargo test --release` must pass; `--no-default-features` (headless) must build. GUI (`gui` feature) needs system tray libs; keep it feature-gated.
- Errors: `anyhow` at boundaries, `thiserror` for library-style errors; no `unwrap`/`expect` on fallible external input.
- Async: no `block_on` inside the runtime; never hold a `std::sync::Mutex` across `.await`; prefer `tokio::fs`/`spawn_blocking` for file I/O in async paths.
- Paths in generated service units (systemd/launchd) must be escaped; per-OS code stays behind `cfg`.
- The tray is persistent; the egui panel is transient (X hides it — winit allows one EventLoop per process — Quit exits).
- Tests for pure logic (snowflakes, schedules, filters, paging, traversal rejection); run `cargo test --release` before committing.

## Layout

- `src/`: `config` (TOML + env), `api` (Discord client), `deletion` (streaming delete + sessions), `archiver`/`archive_format` (V1 format), `scheduler` (per-chat daily/weekly/monthly + run-now), `service` (daemon + native autostart), `ipc` (loopback JSON, secret), `progress` (live job registry), `gui` (tray + panel), `viewer` (axum archive viewer).
- Data dir (`dirs::data_dir()/guillotine`): `archives/`, `deletion_sessions/`, `logs/`, `last_runs.json`, `ipc.port`, `ipc.secret`.
- Config: per-OS config dir `config.toml` (or `--config` / `GUILLOTINE_CONFIG`); token also via `GUILLOTINE_TOKEN`.
