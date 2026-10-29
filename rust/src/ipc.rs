//! Local IPC between the tray app (or CLI) and the running service daemon:
//! a loopback TCP listener whose port is written to the data dir, speaking
//! one JSON object per line. Commands: run-now, status.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::api::Client;
use crate::config::ChatKey;
use crate::config::{self, Config};
use crate::scheduler::{self, ChatJob};

pub fn port_file() -> PathBuf {
    config::data_dir().join("ipc.port")
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
pub async fn serve(cfg: Arc<Config>, registry: crate::progress::SharedRegistry) {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "ipc: cannot bind loopback listener");
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    if let Err(e) = std::fs::write(port_file(), format!("{port}\n")) {
        tracing::warn!(error = %e, "ipc: cannot write port file");
    }
    tracing::info!(port, "ipc listening");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let cfg = cfg.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            let _ = handle(stream, cfg, registry).await;
        });
    }
}

async fn handle(stream: TcpStream, cfg: Arc<Config>, registry: crate::progress::SharedRegistry) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(reader);
    reader.read_line(&mut line).await?;
    let req: Value = serde_json::from_str(line.trim())?;
    let resp = match req.get("cmd").and_then(|c| c.as_str()) {
        Some("status") => json!({ "ok": true, "runs": read_last_runs() }),
        Some("jobs") => json!({ "ok": true, "running": registry.snapshot(), "last": read_last_runs() }),
        Some("run_now") => {
            let chat = req.get("chat").and_then(|c| c.as_str()).unwrap_or("");
            match run_now_via_daemon(&cfg, chat).await {
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

async fn run_now_via_daemon(cfg: &Config, chat: &str) -> anyhow::Result<crate::deletion::Summary> {
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
    let job = ChatJob { key, settings };
    let registry = crate::progress::new_registry();
    let summary = scheduler::run_chat_job(&client, cfg, &job, &registry).await?;
    record_last_run(&job.key.to_string(), &summary);
    Ok(summary)
}

/// Client side: send one command to the running daemon.
pub async fn send(cmd: Value) -> anyhow::Result<Value> {
    let port: u16 = std::fs::read_to_string(port_file())?
        .trim()
        .parse()
        .context("invalid ipc port file — is the service running?")?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    use tokio::io::AsyncWriteExt;
    stream
        .write_all((serde_json::to_string(&cmd)? + "\n").as_bytes())
        .await?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(serde_json::from_str(line.trim())?)
}

use anyhow::Context;
