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

use std::io::{Read as _, Write as _};

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
    let mut raw = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    let secret = raw.iter().map(|b| format!("{b:02x}")).collect::<String>();
    write_private(&path, &format!("{secret}\n"))?;
    Ok(secret)
}

/// Per-chat record of the most recent run, persisted for the tray/status UI.
pub fn last_runs_file() -> PathBuf {
    config::data_dir().join("last_runs.json")
}

pub fn record_last_run(chat: &str, summary: &crate::deletion::Summary) {
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
    if std::fs::write(&tmp, serde_json::to_string_pretty(&runs).unwrap() + "\n").is_ok() {
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
            let _ = handle(stream, Arc::new(fresh), registry, secret).await;
        });
    }
}

async fn handle(
    stream: TcpStream,
    cfg: Arc<Config>,
    registry: crate::progress::SharedRegistry,
    secret: String,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let reader = BufReader::new(reader);
    // Cap the line length and bound the whole exchange so a local peer can
    // neither exhaust memory nor hold a task/fd forever.
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    reader.take(64 * 1024).read_line(&mut line).await?;
    let req: Value = serde_json::from_str(line.trim())?;
    if req.get("secret").and_then(|s| s.as_str()) != Some(secret.as_str()) {
        let resp = json!({ "ok": false, "error": "unauthorized" });
        writer.write_all((serde_json::to_string(&resp)? + "\n").as_bytes()).await?;
        return Ok(());
    }
    let resp = match req.get("cmd").and_then(|c| c.as_str()) {
        Some("status") => json!({ "ok": true, "runs": read_last_runs() }),
        Some("jobs") => json!({ "ok": true, "running": registry.snapshot(), "last": read_last_runs() }),
        Some("run_now") => {
            let chat = req.get("chat").and_then(|c| c.as_str()).unwrap_or("");
            let all = req.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
            match run_now_via_daemon(&cfg, chat, all, &registry).await {
                Ok(summary) => json!({ "ok": true, "summary": {
                    "deleted": summary.deleted, "failed": summary.failed,
                    "skipped": summary.skipped, "scanned": summary.scanned,
                    "complete": summary.complete, "aborted": summary.aborted } }),
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

fn read_last_runs() -> Value {
    std::fs::read_to_string(last_runs_file())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}))
}

async fn run_now_via_daemon(cfg: &Config, chat: &str, delete_all: bool, registry: &crate::progress::SharedRegistry) -> anyhow::Result<crate::deletion::Summary> {
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
    let mut client = Client::new(cfg.token()?)?;
    if client.get_current_user().await?.is_none() {
        anyhow::bail!("invalid token or authentication failed");
    }
    let mut settings = settings;
    if delete_all {
        settings.retention_days = 0; // cutoff = now: everything is deleted
    }
    let job = ChatJob { key, settings };
    let summary = scheduler::run_chat_job(&client, cfg, &job, registry).await?;
    record_last_run(&job.key.to_string(), &summary);
    Ok(summary)
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
