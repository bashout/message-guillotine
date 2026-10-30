//! Local IPC between the tray app (or CLI) and the running service daemon:
//! a loopback TCP listener whose port is written to the data dir, speaking
//! one JSON object per line. Commands: run-now, status.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::api::Client;
use crate::config::ChatKey;
use crate::config::{self, Config};
use crate::scheduler::{self, ChatJob};

pub fn port_file() -> PathBuf {
    config::data_dir().join("ipc.port")
}

/// Shared secret required on every IPC request. Stored 0600 in the data dir.
pub fn secret_file() -> PathBuf {
    config::data_dir().join("ipc.secret")
}

fn write_private(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
            .write_all(contents.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// 32 random bytes hex-encoded from the platform CSPRNG (getrandom) on every
/// platform. Never falls back to a predictable value: a weak IPC secret
/// would let any local process command the daemon.
fn random_secret() -> anyhow::Result<String> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| {
        anyhow::anyhow!("CSPRNG unavailable ({e}); refusing to generate a predictable IPC secret")
    })?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// The daemon's secret: read the existing file, or create a fresh random one.
fn load_or_create_secret() -> anyhow::Result<String> {
    let path = secret_file();
    if let Ok(secret) = std::fs::read_to_string(&path) {
        let secret = secret.trim().to_string();
        if !secret.is_empty() {
            return Ok(secret);
        }
    }
    config::ensure_data_dir()?;
    let secret = random_secret()?;
    write_private(&path, &format!("{secret}\n"))?;
    Ok(secret)
}

/// Per-chat record of the most recent run, persisted for the tray/status UI.
pub fn last_runs_file() -> PathBuf {
    config::data_dir().join("last_runs.json")
}

static LAST_RUNS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn record_last_run(chat: &str, summary: &crate::deletion::Summary) {
    // Serialized: concurrent run-now tasks must not lose each other's records.
    let _g = LAST_RUNS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = last_runs_file();
    let mut runs: Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = runs.as_object_mut() {
        obj.insert(
            chat.to_string(),
            json!({
                "last_run": chrono::Utc::now().to_rfc3339(),
                "deleted": summary.deleted,
                "failed": summary.failed,
                "skipped": summary.skipped,
                "scanned": summary.scanned,
                "complete": summary.complete,
                "aborted": summary.aborted,
            }),
        );
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = file.with_extension("json.part");
    if crate::config::write_file_private(&tmp, &(serde_json::to_string_pretty(&runs).unwrap() + "\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &file);
    }
}

/// Serve IPC requests until the process exits. Spawned by the daemon.
pub async fn serve(
    cfg: Arc<Config>,
    registry: crate::progress::SharedRegistry,
    config_path: std::path::PathBuf,
) {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "ipc: cannot bind loopback listener");
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    if let Err(e) = config::ensure_data_dir().and_then(|_| write_private(&port_file(), &format!("{port}\n")).map_err(std::io::Error::other)) {
        tracing::warn!(error = %e, "ipc: cannot write port file");
    }
    let secret = match load_or_create_secret() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "ipc: cannot create secret; refusing to serve unauthenticated");
            return;
        }
    };
    tracing::info!(port, "ipc listening");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let cfg = cfg.clone();
        let registry = registry.clone();
        let secret = secret.clone();
        let config_path = config_path.clone();
        tokio::spawn(async move {
            // Reload the config per request: edits made in the GUI settings
            // apply to run-now immediately, without a daemon restart.
            let fresh = Config::load(&config_path).unwrap_or_else(|_| (*cfg).clone());
            // Bound the whole exchange: a peer that connects and sends nothing
            // must not pin a task + fd forever.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                handle(stream, Arc::new(fresh), registry, secret),
            )
            .await;
        });
    }
}

/// Request lines are capped at 64 KiB: a local peer can neither exhaust
/// memory nor hold a task/fd forever with an endless line.
const MAX_REQUEST_LINE: u64 = 64 * 1024;

/// Read one request line, capped at [`MAX_REQUEST_LINE`] bytes.
async fn read_request_line<R>(reader: R) -> anyhow::Result<String>
where
    R: tokio::io::AsyncBufReadExt + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut line = String::new();
    reader.take(MAX_REQUEST_LINE).read_line(&mut line).await?;
    Ok(line)
}

/// Response payload for `cmd: "status"`.
fn status_response() -> Value {
    json!({ "ok": true, "runs": read_last_runs() })
}

/// Response payload for `cmd: "jobs"`: the live registry plus last runs.
fn jobs_response(registry: &crate::progress::SharedRegistry) -> Value {
    json!({ "ok": true, "running": registry.snapshot(), "last": read_last_runs() })
}

async fn handle(
    stream: TcpStream,
    cfg: Arc<Config>,
    registry: crate::progress::SharedRegistry,
    secret: String,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    // Cap the line length and bound the whole exchange so a local peer can
    // neither exhaust memory nor hold a task/fd forever.
    let line = read_request_line(BufReader::new(reader)).await?;
    let req: Value = serde_json::from_str(line.trim())?;
    if !secret_eq(req.get("secret").and_then(|s| s.as_str()).unwrap_or(""), &secret) {
        let resp = json!({ "ok": false, "error": "unauthorized" });
        writer.write_all((serde_json::to_string(&resp)? + "\n").as_bytes()).await?;
        return Ok(());
    }
    let resp = match req.get("cmd").and_then(|c| c.as_str()) {
        Some("status") => status_response(),
        Some("jobs") => jobs_response(&registry),
        Some("archive") => {
            let channel = req.get("channel").and_then(|c| c.as_str()).unwrap_or("").to_string();
            if !crate::config::valid_id(&channel) {
                json!({ "ok": false, "error": "channel id must be numeric" })
            } else {
                let cfg = cfg.clone();
                let channel_job = channel.clone();
                tokio::spawn(async move {
                    let channel = channel_job;
                    let _guard = crate::deletion::deletion_lock().lock().await;
                    let client = match crate::api::Client::new(cfg.token().unwrap_or("")) {
                        Ok(mut c) => { let _ = c.get_current_user().await; c }
                        Err(e) => { tracing::warn!(error = %e, "archive: client"); return; }
                    };
                    let dir = cfg.resolve_archive_dir(None);
                    if let Err(e) = crate::archiver::archive_chat(&client, &channel, &channel, &dir).await {
                        tracing::warn!(channel = %channel, error = %e, "archive failed");
                    }
                });
                json!({ "ok": true, "accepted": true, "channel": channel })
            }
        }
        Some("run_now") => {
            let chat = req.get("chat").and_then(|c| c.as_str()).unwrap_or("").to_string();
            let all = req.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
            // Ack immediately and run in the background: a deletion can take
            // many minutes at the 24/min pace, and the client must not time
            // out (or worse, fall back to an unlocked second run). The job
            // still serializes through the daemon's deletion lock.
            match prepare_run_now(&cfg, &chat, all) {
                Ok(job) => {
                    let cfg = cfg.clone();
                    let registry = registry.clone();
                    let chat_log = chat.clone();
                    tokio::spawn(async move {
                        match run_now_via_daemon(&cfg, job, &registry).await {
                            Ok(summary) => {
                                tracing::info!(chat = %chat_log, deleted = summary.deleted, "run-now finished");
                                record_last_run(&chat_log, &summary);
                            }
                            Err(e) => tracing::warn!(chat = %chat_log, error = %e, "run-now failed"),
                        }
                    });
                    json!({ "ok": true, "accepted": true, "chat": chat })
                }
                Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
            }
        }
        other => json!({ "ok": false, "error": format!("unknown cmd {other:?}") }),
    };
    writer
        .write_all((serde_json::to_string(&resp)? + "\n").as_bytes())
        .await?;
    Ok(())
}

pub fn read_last_runs_public() -> serde_json::Map<String, serde_json::Value> {
    read_last_runs().as_object().cloned().unwrap_or_default()
}

/// Constant-time string compare (avoids a timing oracle on the IPC secret).
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for i in 0..a.len() {
        acc |= a[i] ^ b[i];
    }
    acc == 0
}

fn read_last_runs() -> Value {
    std::fs::read_to_string(last_runs_file())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}))
}

/// Validate and build the job for a run-now request (no API calls).
fn prepare_run_now(cfg: &Config, chat: &str, delete_all: bool) -> anyhow::Result<ChatJob> {
    let key = ChatKey::parse(chat)?;
    let settings = match &key {
        ChatKey::Dm(user) => cfg
            .dm_chat_settings(user)
            .ok_or_else(|| anyhow::anyhow!("dm user {user} is not in scope"))?,
        ChatKey::GuildChannel { .. } => cfg
            .chat
            .get(&key.to_string())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("chat {key} has no [chat] entry"))?,
    };
    let mut settings = settings;
    if delete_all {
        settings.retention_days = 0; // cutoff = now: everything is deleted
    }
    Ok(ChatJob { key, settings })
}

async fn run_now_via_daemon(cfg: &Config, job: ChatJob, registry: &crate::progress::SharedRegistry) -> anyhow::Result<crate::deletion::Summary> {
    let mut client = Client::new(cfg.token()?)?;
    if client.get_current_user().await?.is_none() {
        anyhow::bail!("invalid token or authentication failed");
    }
    scheduler::run_chat_job(&client, cfg, &job, registry).await
}

/// Client side: send one command to the running daemon.
pub async fn send(mut cmd: Value) -> anyhow::Result<Value> {
    let secret = std::fs::read_to_string(secret_file()).context("no ipc secret — is the service running?")?;
    cmd["secret"] = Value::String(secret.trim().to_string());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let port: u16 = std::fs::read_to_string(port_file())?
            .trim()
            .parse()
            .context("invalid ipc port file — is the service running?")?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream
            .write_all((serde_json::to_string(&cmd)? + "\n").as_bytes())
            .await?;
        let reader = BufReader::new(stream);
        let mut line = String::new();
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        reader.take(1024 * 1024).read_line(&mut line).await?;
        Ok(serde_json::from_str(line.trim())?)
    })
    .await
    .context("ipc request timed out")?
}

use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(toml_body: &str) -> Config {
        toml::from_str(toml_body).unwrap()
    }

    #[test]
    fn random_secret_is_64_hex_chars_and_unique() {
        let a = random_secret().unwrap();
        let b = random_secret().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two secrets must never collide");
    }

    #[test]
    fn secret_eq_constant_time_semantics() {
        assert!(secret_eq("abc123", "abc123"));
        assert!(!secret_eq("abc123", "abc124"));
        assert!(!secret_eq("abc123", "abc12"));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
        assert!(!secret_eq("ABC", "abc"));
    }

    #[tokio::test]
    async fn request_line_is_capped_at_64kib() {
        // a well-formed short line reads fully
        let short = b"{\"cmd\":\"status\"}\n".to_vec();
        let line = read_request_line(&mut &short[..]).await.unwrap();
        assert_eq!(line, "{\"cmd\":\"status\"}\n");
        // an over-cap line is truncated at the cap and therefore unparseable
        let mut big = b"{\"pad\":\"".to_vec();
        big.extend(std::iter::repeat_n(b'a', 200 * 1024));
        big.extend_from_slice(b"\"}\n");
        let line = read_request_line(&mut &big[..]).await.unwrap();
        assert_eq!(line.len() as u64, MAX_REQUEST_LINE);
        assert!(serde_json::from_str::<Value>(line.trim()).is_err());
        // at the cap without a newline, the line is cut at exactly the cap
        let exact = vec![b' '; MAX_REQUEST_LINE as usize];
        let line = read_request_line(&mut &exact[..]).await.unwrap();
        assert_eq!(line.len() as u64, MAX_REQUEST_LINE);
    }

    #[test]
    fn prepare_run_now_validates_chat_key() {
        let cfg = cfg_with("token = \"t\"\n[dm]\npolicy = \"nominate\"\nusers = [\"1\"]\n");
        assert!(prepare_run_now(&cfg, "not-a-key", false).is_err());
        assert!(prepare_run_now(&cfg, "dm:../etc", false).is_err());
        assert!(prepare_run_now(&cfg, "guild:1/channel:2/3", false).is_err());
    }

    #[test]
    fn prepare_run_now_dm_scope_and_all_flag() {
        let cfg = cfg_with(
            "token = \"t\"\n[dm]\npolicy = \"nominate\"\nusers = [\"1\"]\ndefault_retention_days = 30\n\
             [chat.\"dm:1\"]\nschedule = \"weekly\"\nretention_days = 7\n",
        );
        // nominated user with own entry
        let job = prepare_run_now(&cfg, "dm:1", false).unwrap();
        assert_eq!(job.settings.retention_days, 7);
        assert_eq!(job.settings.schedule.kind, crate::config::ScheduleKind::Weekly);
        // all=true forces retention 0 (delete everything)
        let job = prepare_run_now(&cfg, "dm:1", true).unwrap();
        assert_eq!(job.settings.retention_days, 0);
        // user not in scope under nominate policy
        assert!(prepare_run_now(&cfg, "dm:2", false).is_err());
        // guild channel without a [chat] entry
        assert!(prepare_run_now(&cfg, "guild:1/channel:2", false).is_err());

        let cfg = cfg_with(
            "token = \"t\"\n[dm]\npolicy = \"exclude\"\nusers = [\"9\"]\ndefault_retention_days = 30\n",
        );
        // exclude policy: any other user is in scope with dm defaults
        let job = prepare_run_now(&cfg, "dm:2", false).unwrap();
        assert_eq!(job.settings.retention_days, 30);
        // excluded user is out of scope
        assert!(prepare_run_now(&cfg, "dm:9", false).is_err());
    }

    #[test]
    fn jobs_and_status_response_shape() {
        let registry = crate::progress::new_registry();
        registry.start("dm:1", "Alice (@alice)", "deleting");
        registry.update("dm:1", |p| p.deleted = 5);
        let resp = jobs_response(&registry);
        assert_eq!(resp["ok"], true);
        let running = resp["running"].as_array().unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0]["chat"], "dm:1");
        assert_eq!(running[0]["label"], "Alice (@alice)");
        assert_eq!(running[0]["phase"], "deleting");
        assert_eq!(running[0]["deleted"], 5);
        assert!(running[0]["started"].as_str().is_some());
        assert!(resp["last"].is_object());
        // status has no "running" key
        let resp = status_response();
        assert_eq!(resp["ok"], true);
        assert!(resp.get("running").is_none());
        assert!(resp["runs"].is_object());
    }
}
