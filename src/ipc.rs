//! Local IPC between the tray app (or CLI) and the running service daemon:
//! a loopback TCP listener whose port is written to the data dir, speaking
//! one JSON object per line. Commands: run-now, status.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::ChatKey;
use crate::config::{self, Config};
use crate::scheduler::{self, ChatJob};

pub fn port_file() -> PathBuf {
    config::data_dir().join("ipc.port")
}

/// Whether a daemon is currently listening: the port file's port answers the
/// secretless hello handshake. Used by the default launch (and `gui`) to
/// decide whether the service daemon needs starting. No secret is involved,
/// so an unrelated process squatting a reused port is never mistaken for the
/// daemon — and never learns anything from the probe.
/// (Only the gui feature calls this; headless builds start no tray.)
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn daemon_alive() -> bool {
    let Ok(raw) = std::fs::read_to_string(port_file()) else {
        return false;
    };
    let Ok(port) = raw.trim().parse::<u16>() else {
        return false;
    };
    hello_ok(port)
}

/// Say hello to whatever listens on `port`: true only when a daemon answers
/// the handshake (`ok: true`). Bounded by short timeouts so a wedged peer
/// cannot stall the caller. Reads exactly one response line — the daemon
/// keeps the connection open for further exchanges (the handshake and the
/// secret-authenticated command share one connection), so waiting for EOF
/// would hang until the timeout.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn hello_ok(port: u16) -> bool {
    use std::io::{BufRead, Write};
    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    if stream.write_all(b"{\"cmd\":\"hello\"}\n").is_err() {
        return false;
    }
    let mut line = String::new();
    if std::io::BufReader::new(&mut stream)
        .read_line(&mut line)
        .is_err()
    {
        return false;
    }
    serde_json::from_str(line.trim())
        .map(|v: Value| v.get("ok").and_then(Value::as_bool) == Some(true))
        .unwrap_or(false)
}

/// Shared secret required on every IPC request. Stored 0600 in the data dir.
pub fn secret_file() -> PathBuf {
    config::data_dir().join("ipc.secret")
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
    config::write_file_private(&path, &format!("{secret}\n"))?;
    Ok(secret)
}

/// Per-chat record of the most recent run, persisted for the tray/status UI.
pub fn last_runs_file() -> PathBuf {
    config::data_dir().join("last_runs.json")
}

static LAST_RUNS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub async fn record_last_run(chat: &str, summary: &crate::deletion::Summary) {
    // Serialized: concurrent run-now tasks must not lose each other's records.
    // The read+merge+write runs on the blocking pool; the std Mutex is never
    // held across an .await.
    let file = last_runs_file();
    let chat = chat.to_string();
    let entry = json!({
        "last_run": chrono::Utc::now().to_rfc3339(),
        "deleted": summary.deleted,
        "failed": summary.failed,
        "skipped": summary.skipped,
        "scanned": summary.scanned,
        "complete": summary.complete,
        "aborted": summary.aborted,
    });
    let payload = {
        let file = file.clone();
        match tokio::task::spawn_blocking(move || {
            let _g = LAST_RUNS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut runs: Value = std::fs::read_to_string(&file)
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or_else(|| json!({}));
            if let Some(obj) = runs.as_object_mut() {
                obj.insert(chat, entry);
            }
            serde_json::to_string_pretty(&runs).unwrap() + "\n"
        })
        .await
        {
            Ok(p) => p,
            Err(_) => return, // blocking task panicked: nothing sensible to record
        }
    };
    if let Some(parent) = file.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let tmp = file.with_extension("json.part");
    let _ = tokio::task::spawn_blocking(move || {
        if crate::config::write_file_private(&tmp, &payload).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    })
    .await;
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
    if let Err(e) = config::ensure_data_dir()
        .and_then(|_| config::write_file_private(&port_file(), &format!("{port}\n")))
    {
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
    json!({ "ok": true, "auth": scheduler::auth_state_str(), "runs": read_last_runs() })
}

/// Response payload for `cmd: "jobs"`: the live registry plus last runs.
fn jobs_response(registry: &crate::progress::SharedRegistry) -> Value {
    json!({
        "ok": true,
        "auth": scheduler::auth_state_str(),
        "running": registry.snapshot(),
        "last": read_last_runs()
    })
}

async fn handle(
    stream: TcpStream,
    cfg: Arc<Config>,
    registry: crate::progress::SharedRegistry,
    secret: String,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    // One JSON request per line, several exchanges per connection: the
    // client opens with the secretless hello handshake (proving this
    // listener is the daemon BEFORE any secret is sent) and then sends its
    // secret-authenticated command(s) on the same connection — the proof
    // and the secret must share the connection, otherwise a port squatter
    // between two connections would receive the secret. The loop ends when
    // either side closes, a line is unparseable (no response, so an
    // over-cap or garbage line is never answered), or an error aborts the
    // exchange; serve() bounds the whole connection in time.
    loop {
        let line = read_request_line(&mut reader).await?;
        if line.trim().is_empty() {
            return Ok(()); // client closed the connection
        }
        let req: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => return Ok(()), // over-cap or garbage: end without a response
        };
        // Handshake: requires no secret itself, and the connection stays
        // open for the command that follows.
        if req.get("cmd").and_then(|c| c.as_str()) == Some("hello") {
            let resp = json!({ "ok": true, "proto": 1 });
            writer
                .write_all((serde_json::to_string(&resp)? + "\n").as_bytes())
                .await?;
            continue;
        }
        if !secret_eq(
            req.get("secret").and_then(|s| s.as_str()).unwrap_or(""),
            &secret,
        ) {
            let resp = json!({ "ok": false, "error": "unauthorized" });
            writer
                .write_all((serde_json::to_string(&resp)? + "\n").as_bytes())
                .await?;
            return Ok(());
        }
        let resp = match req.get("cmd").and_then(|c| c.as_str()) {
            Some("status") => status_response(),
            Some("jobs") => jobs_response(&registry),
            Some("reauth") => {
                // Acks immediately: the daemon's auth loop wakes on its own and
                // the tray follows the outcome via the 1 s `jobs` poll (the
                // "auth" field flips to ok/retrying/failed).
                scheduler::request_reauth();
                json!({ "ok": true, "accepted": true })
            }
            Some("shutdown") => {
                // Clean daemon shutdown: same path as SIGINT/SIGTERM (flush
                // logs, remove the port file). Acks immediately; the daemon
                // exits right after. Used by the tray's Stop service item and
                // Quit, so closing the app cleans up the service instance.
                crate::service::request_shutdown();
                json!({ "ok": true, "accepted": true })
            }
            Some("archive") => {
                let channel = req
                    .get("channel")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if !crate::config::valid_id(&channel) {
                    json!({ "ok": false, "error": "channel id must be numeric" })
                } else {
                    let cfg = cfg.clone();
                    let channel_job = channel.clone();
                    tokio::spawn(async move {
                        let channel = channel_job;
                        let _guard = crate::deletion::deletion_lock().lock().await;
                        // Auth failure must stop the archive, not run it unauthenticated.
                        let client = match crate::api::authed_client(&cfg).await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!(error = %e, "archive: authentication failed");
                                return;
                            }
                        };
                        let dir = cfg.resolve_archive_dir(None);
                        if let Err(e) =
                            crate::archiver::archive_chat(&client, &channel, &channel, &dir).await
                        {
                            tracing::warn!(channel = %channel, error = %e, "archive failed");
                        }
                    });
                    json!({ "ok": true, "accepted": true, "channel": channel })
                }
            }
            Some("run_now") => {
                let chat = req
                    .get("chat")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
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
                                    record_last_run(&chat_log, &summary).await;
                                }
                                Err(e) => {
                                    tracing::warn!(chat = %chat_log, error = %e, "run-now failed")
                                }
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
    }
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
    let mut settings = scheduler::resolve_chat_settings(cfg, &key)?;
    if delete_all {
        settings.retention_days = 0; // cutoff = now: everything is deleted
    }
    Ok(ChatJob { key, settings })
}

async fn run_now_via_daemon(
    cfg: &Config,
    job: ChatJob,
    registry: &crate::progress::SharedRegistry,
) -> anyhow::Result<crate::deletion::Summary> {
    let client = crate::api::authed_client(cfg).await?;
    scheduler::run_chat_job(&client, cfg, &job, registry).await
}

/// Why an IPC send failed. [`SendError::Absent`] means no daemon is listening
/// (missing port/secret files, connection refused) and it is safe to fall
/// back to in-process work; [`SendError::Failed`] means a daemon may be alive
/// but wedged — running unlocked then would break the single-deletion
/// guarantee, so callers must error out instead of falling back.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("{0}")]
    Absent(anyhow::Error),
    #[error("{0}")]
    Failed(anyhow::Error),
}

/// Client side: send one command to the running daemon. Distinguishes "no
/// daemon running" ([`SendError::Absent`]) from "daemon alive but not
/// answering" ([`SendError::Failed`]).
pub async fn send_typed(mut cmd: Value) -> Result<Value, SendError> {
    let secret = std::fs::read_to_string(secret_file())
        .map_err(|e| SendError::Absent(anyhow::anyhow!("no ipc secret file: {e}")))?;
    cmd["secret"] = Value::String(secret.trim().to_string());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let port: u16 = std::fs::read_to_string(port_file())
            .map_err(|e| SendError::Absent(anyhow::anyhow!("no ipc port file: {e}")))?
            .trim()
            .parse()
            .map_err(|_| SendError::Absent(anyhow::anyhow!("invalid ipc port file")))?;
        let mut stream = match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err(SendError::Absent(anyhow::anyhow!(
                    "connection refused on port {port}: no daemon running"
                )));
            }
            Err(e) => return Err(SendError::Failed(e.into())),
        };
        // Handshake first: the secret is only sent once the peer has proven it
        // is the daemon, so a squatter on a reused ephemeral port never gets it.
        stream
            .write_all(b"{\"cmd\":\"hello\"}\n")
            .await
            .map_err(|e| SendError::Failed(e.into()))?;
        {
            use tokio::io::AsyncBufReadExt as _;
            let mut hello = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut hello)
                .await
                .map_err(|e| SendError::Failed(e.into()))?;
            let hello: Value =
                serde_json::from_str(hello.trim()).map_err(|e| SendError::Failed(e.into()))?;
            if hello.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                return Err(SendError::Failed(anyhow::anyhow!(
                    "peer on port {port} is not the guillotine daemon"
                )));
            }
        }
        stream
            .write_all(
                (serde_json::to_string(&cmd).map_err(|e| SendError::Failed(e.into()))? + "\n")
                    .as_bytes(),
            )
            .await
            .map_err(|e| SendError::Failed(e.into()))?;
        let reader = BufReader::new(stream);
        let mut line = String::new();
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        reader
            .take(1024 * 1024)
            .read_line(&mut line)
            .await
            .map_err(|e| SendError::Failed(e.into()))?;
        serde_json::from_str(line.trim()).map_err(|e| SendError::Failed(e.into()))
    })
    .await
    .map_err(|_| SendError::Failed(anyhow::anyhow!("ipc request timed out")))?
}

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
        assert_eq!(
            job.settings.schedule.kind,
            crate::config::ScheduleKind::Weekly
        );
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
        assert!(resp["auth"].is_string(), "auth state must be reported");
        let running = resp["running"].as_array().unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0]["chat"], "dm:1");
        assert_eq!(running[0]["label"], "Alice (@alice)");
        assert_eq!(running[0]["phase"], "deleting");
        assert_eq!(running[0]["deleted"], 5);
        assert!(running[0]["started"].as_str().is_some());
        assert!(resp["last"].is_object());
        // status has no "running" key but does report auth
        let resp = status_response();
        assert_eq!(resp["ok"], true);
        assert!(resp["auth"].is_string());
        assert!(resp.get("running").is_none());
        assert!(resp["runs"].is_object());
    }

    #[tokio::test]
    async fn ipc_reauth_acks_immediately() {
        // Acks without doing the auth itself; the daemon's auth loop wakes
        // on its own (request_reauth is fire-and-forget, no token involved).
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let resp: Value = serde_json::from_str(
            &ipc_send(port, r#"{"cmd":"reauth","secret":"test-secret"}"#).await,
        )
        .unwrap();
        assert_eq!(resp, json!({"ok": true, "accepted": true}));
    }

    #[tokio::test]
    async fn ipc_shutdown_acks_immediately() {
        // Acks without exiting anything: the test's spawn_ipc listener is
        // not the real daemon, and request_shutdown is fire-and-forget —
        // only a daemon running service::run acts on it.
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let resp: Value = serde_json::from_str(
            &ipc_send(port, r#"{"cmd":"shutdown","secret":"test-secret"}"#).await,
        )
        .unwrap();
        assert_eq!(resp, json!({"ok": true, "accepted": true}));
    }

    // ------------------------------------------------------------------
    // Loopback integration: handle() over a real 127.0.0.1 TCP socket.
    // ------------------------------------------------------------------

    async fn spawn_ipc(secret: &str, cfg: Config) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = Arc::new(cfg);
        let secret = secret.to_string();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let cfg = cfg.clone();
                let secret = secret.clone();
                tokio::spawn(async move {
                    let registry = crate::progress::new_registry();
                    let _ = handle(stream, cfg, registry, secret).await;
                });
            }
        });
        port
    }

    async fn ipc_send(port: u16, line: &str) -> String {
        use tokio::io::AsyncBufReadExt as _;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut resp = String::new();
        let n = reader.read_line(&mut resp).await.unwrap();
        if n == 0 {
            return String::new(); // EOF: no response written
        }
        resp.trim().to_string()
    }

    fn ipc_cfg() -> Config {
        // The token contains a newline so the run-now background task's
        // Client::new fails immediately — the test never touches the network.
        toml::from_str(
            "token = \"bad\\ntoken\"\n[chat.\"guild:1/channel:2\"]\nschedule = \"daily\"\nretention_days = 7\n",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn hello_handshake_needs_no_secret() {
        // The client sends hello before its secret, so a port squatter never
        // receives the secret; the daemon must answer ok without one.
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let resp: Value =
            serde_json::from_str(&ipc_send(port, r#"{"cmd":"hello"}"#).await).unwrap();
        assert_eq!(resp.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn ipc_missing_or_wrong_secret_is_unauthorized() {
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let resp: Value =
            serde_json::from_str(&ipc_send(port, r#"{"cmd":"status"}"#).await).unwrap();
        assert_eq!(resp, json!({"ok": false, "error": "unauthorized"}));
        let resp: Value =
            serde_json::from_str(&ipc_send(port, r#"{"cmd":"status","secret":"nope"}"#).await)
                .unwrap();
        assert_eq!(resp, json!({"ok": false, "error": "unauthorized"}));
        // empty secret is not the secret
        let resp: Value =
            serde_json::from_str(&ipc_send(port, r#"{"cmd":"status","secret":""}"#).await).unwrap();
        assert_eq!(resp["ok"], false);
    }

    #[tokio::test]
    async fn ipc_status_with_correct_secret() {
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let resp: Value = serde_json::from_str(
            &ipc_send(port, r#"{"cmd":"status","secret":"test-secret"}"#).await,
        )
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert!(resp["runs"].is_object());
    }

    #[tokio::test]
    async fn ipc_unknown_cmd_is_an_error() {
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let resp: Value = serde_json::from_str(
            &ipc_send(port, r#"{"cmd":"explode","secret":"test-secret"}"#).await,
        )
        .unwrap();
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().unwrap().contains("unknown cmd"));
    }

    #[tokio::test]
    async fn ipc_over_64kib_line_gets_no_response() {
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let mut big = String::from(r#"{"cmd":"status","secret":"test-secret","pad":""#);
        big.push_str(&"a".repeat(100 * 1024));
        big.push_str("\"}");
        // truncated at the 64 KiB cap -> unparseable -> handler errors out and
        // the connection closes without a response line. When the server
        // closes early, the client's write may fail with ECONNRESET (seen on
        // macOS) instead of succeeding into the kernel buffer (Linux) — both
        // mean the over-cap line was never answered.
        use tokio::io::AsyncBufReadExt as _;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let _ = stream.write_all(big.as_bytes()).await;
        let _ = stream.write_all(b"\n").await;
        let mut resp = String::new();
        let n = match BufReader::new(stream).read_line(&mut resp).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => 0,
            Err(e) => panic!("unexpected read error: {e}"),
        };
        assert!(
            n == 0 && resp.is_empty(),
            "over-cap line must not be answered: {resp:?}"
        );
    }

    #[tokio::test]
    async fn ipc_run_now_acks_immediately() {
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        // The reply must be the immediate ack, not a deletion result: the
        // background job cannot even build a client with this config's token.
        let resp: Value = serde_json::from_str(
            &ipc_send(
                port,
                r#"{"cmd":"run_now","chat":"guild:1/channel:2","secret":"test-secret"}"#,
            )
            .await,
        )
        .unwrap();
        assert_eq!(
            resp,
            json!({"ok": true, "accepted": true, "chat": "guild:1/channel:2"})
        );
        // an invalid chat key is rejected synchronously
        let resp: Value = serde_json::from_str(
            &ipc_send(
                port,
                r#"{"cmd":"run_now","chat":"not-a-key","secret":"test-secret"}"#,
            )
            .await,
        )
        .unwrap();
        assert_eq!(resp["ok"], false);
    }

    // ------------------------------------------------------------------
    // daemon_alive / hello_ok: the blocking liveness probe.
    // ------------------------------------------------------------------

    /// A std::net listener thread that consumes the request line like the
    /// real daemon, then answers every connection with one fixed line.
    /// (Reading first matters: closing a socket with unread inbound data
    /// sends RST instead of FIN, which would make the probe fail.)
    fn echo_listener(line: &'static str) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // consume the request line
                let _ = stream.write_all(line.as_bytes());
                let _ = stream.write_all(b"\n");
            }
        });
        port
    }

    #[test]
    fn hello_ok_recognizes_a_daemon_handshake() {
        // the daemon's hello answer proves the peer
        let port = echo_listener("{\"ok\": true, \"proto\": 1}");
        assert!(hello_ok(port));
        // a non-daemon peer on the port is rejected
        let port = echo_listener("{\"ok\": false}");
        assert!(!hello_ok(port));
        // garbage on the wire is rejected
        let port = echo_listener("not json");
        assert!(!hello_ok(port));
        // nothing listening: connection refused
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!hello_ok(dead_port));
    }

    #[tokio::test]
    async fn daemon_alive_over_the_real_daemon_handshake() {
        // spawn_ipc answers the same hello handshake the real daemon does.
        // The probe blocks its thread, so it must run on the blocking pool:
        // on the current-thread test runtime it would starve the listener.
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let alive = tokio::task::spawn_blocking(move || hello_ok(port))
            .await
            .unwrap();
        assert!(alive);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!hello_ok(dead_port));
    }

    /// The client protocol the tray depends on: the secretless hello
    /// handshake and the secret-authenticated command share ONE
    /// connection — the proof that the listener is the daemon must happen
    /// on the same connection the secret is sent on, otherwise a squatter
    /// between two connections would receive the secret. Regression test
    /// for the server closing the connection after answering hello: the
    /// panel's polls then all failed (EOF / os error 10053) while every
    /// single-command test kept passing.
    #[tokio::test]
    async fn hello_then_command_on_the_same_connection_round_trips() {
        use tokio::io::AsyncBufReadExt as _;
        let port = spawn_ipc("test-secret", ipc_cfg()).await;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"{\"cmd\":\"hello\"}\n").await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut hello = String::new();
        reader.read_line(&mut hello).await.unwrap();
        assert_eq!(hello.trim(), r#"{"ok":true,"proto":1}"#);
        // the daemon must keep serving this connection after the handshake
        w.write_all(b"{\"cmd\":\"jobs\",\"secret\":\"test-secret\"}\n")
            .await
            .unwrap();
        let mut resp = String::new();
        reader.read_line(&mut resp).await.unwrap();
        let resp: Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(resp["ok"], true);
        assert!(resp["running"].is_array());
        assert!(resp["last"].is_object());
        assert!(resp["auth"].is_string());
        // a second command on the same connection works too
        w.write_all(b"{\"cmd\":\"status\",\"secret\":\"test-secret\"}\n")
            .await
            .unwrap();
        let mut resp2 = String::new();
        reader.read_line(&mut resp2).await.unwrap();
        let resp2: Value = serde_json::from_str(resp2.trim()).unwrap();
        assert_eq!(resp2["ok"], true);
        assert!(resp2["runs"].is_object());
    }
}
