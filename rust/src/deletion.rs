//! Streaming deletion runs with resumable per-channel state, plus the
//! process-wide lock that guarantees only one deletion runs at a time
//! (scheduled jobs and manual "run now" alike), so deletes never stack
//! beyond the platform's self-bot pacing.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{self, Client, DeleteOutcome};
use crate::config;

const SAMPLE_MAX: usize = 3;

/// One deletion run at a time, process-wide. Every deletion path (scheduled
/// job, run-now, one-shot scan) acquires this before touching the API's
/// delete bucket.
pub fn deletion_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub channel_id: String,
    pub label: String,
    /// ISO start bound (older messages are out of range), if any.
    pub start_time: Option<String>,
    /// ISO end bound (newer messages are out of range), if any.
    pub end_time: Option<String>,
    pub before_cursor: Option<String>,
    pub scanned: u64,
    pub deleted: u64,
    pub failed: u64,
    pub skipped: u64,
    #[serde(default)]
    pub complete: bool,
}

pub fn session_path(channel_id: &str, root: Option<&Path>) -> PathBuf {
    let root = root
        .map(Path::to_path_buf)
        .unwrap_or_else(config::sessions_dir);
    root.join(format!("{channel_id}.json"))
}

/// The unfinished session for this channel, or None (a complete or corrupt
/// session file counts as absent).
pub fn load_session(channel_id: &str, root: Option<&Path>) -> Option<Session> {
    let state: Session =
        serde_json::from_str(&std::fs::read_to_string(session_path(channel_id, root)).ok()?)
            .ok()?;
    if state.complete {
        return None;
    }
    Some(state)
}

/// Persist run state atomically (temp file + rename). Resume is a
/// convenience: if the write fails the deletes already stand, the run just
/// rescans further next time.
pub fn save_session(channel_id: &str, state: &Session, root: Option<&Path>) {
    let path = session_path(channel_id, root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.part");
    if std::fs::write(&tmp, serde_json::to_string(state).unwrap() + "\n").is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub fn clear_session(channel_id: &str, root: Option<&Path>) {
    let _ = std::fs::remove_file(session_path(channel_id, root));
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub channel_id: String,
    pub scanned: u64,
    pub deleted: u64,
    pub failed: u64,
    pub skipped: u64,
    pub complete: bool,
    pub aborted: Option<String>,
    pub samples: Vec<String>,
}

fn sample(msg: &api::Message) -> String {
    let name = msg
        .author
        .global_name
        .clone()
        .unwrap_or_else(|| msg.author.username.clone());
    let content = msg.content.split_whitespace().collect::<Vec<_>>().join(" ");
    let content = if content.len() > 60 {
        format!("{}...", &content[..57])
    } else {
        content
    };
    format!("{name}: {content}")
}

/// Stream-scan the channel newest-to-oldest and delete every message of the
/// current user older than `cutoff` (None = everything). With `resume`, the
/// scan continues after the session's before-cursor and counters carry over.
/// With `dry_run`, messages are only counted and quoted — nothing is deleted
/// and no session is written.
pub async fn run_deletion(
    client: &Client,
    channel_id: &str,
    label: &str,
    cutoff: Option<DateTime<Utc>>,
    resume: Option<Session>,
    dry_run: bool,
    root: Option<&Path>,
    registry: Option<&crate::progress::SharedRegistry>,
) -> Summary {
    let channel_id = channel_id.to_string();
    let cutoff_ts = cutoff.map(|c| c.timestamp() as f64);

    let mut state = Session {
        channel_id: channel_id.clone(),
        label: label.to_string(),
        start_time: None,
        end_time: cutoff.map(|c| c.to_rfc3339()),
        before_cursor: None,
        scanned: 0,
        deleted: 0,
        failed: 0,
        skipped: 0,
        complete: false,
    };
    if let Some(resume) = resume {
        state.scanned = resume.scanned;
        state.deleted = resume.deleted;
        state.failed = resume.failed;
        state.skipped = resume.skipped;
        state.before_cursor = resume.before_cursor;
    } else if let Some(cutoff) = cutoff {
        // Jump straight to the range boundary instead of scanning from the
        // channel top: `before` the cutoff's snowflake (+1ms to keep the
        // bound inclusive) starts the first page inside the range.
        let bound = cutoff + chrono::Duration::milliseconds(1);
        state.before_cursor = Some(api::snowflake_for(bound));
    }

    let me_id = client.current_user.as_ref().map(|u| u.id.clone());
    let mut samples = Vec::new();
    let mut aborted: Option<String> = None;
    let mut pending_pace = Duration::ZERO;
    let start_clock = std::time::Instant::now();

    let result = async {
        let mut cursor = state.before_cursor.clone();
        loop {
            let page = match client
                .history_page_before(&channel_id, cursor.as_deref())
                .await
            {
                Ok(p) => p,
                Err(e) => return Err(format!("fetch failed: {e}")),
            };
            if page.is_empty() {
                return Ok(());
            }
            for msg in &page {
                if let Some(cut) = cutoff_ts {
                    if msg.epoch().unwrap_or(0.0) > cut {
                        continue; // newer than the cutoff: keep scanning backwards
                    }
                }
                state.scanned += 1;
                state.before_cursor = Some(msg.id.clone());
                if let Some(reg) = registry {
                    reg.update(&channel_id, |p| {
                        p.scanned = state.scanned;
                        p.deleted = state.deleted;
                        p.failed = state.failed;
                        p.skipped = state.skipped;
                    });
                }

                if Some(&msg.author.id) != me_id.as_ref() {
                    if !dry_run {
                        save_session(&channel_id, &state, root);
                    }
                    continue;
                }
                if msg.is_system() {
                    state.skipped += 1;
                    if !dry_run {
                        save_session(&channel_id, &state, root);
                    }
                    continue;
                }
                if dry_run {
                    state.deleted += 1;
                    if samples.len() < SAMPLE_MAX {
                        samples.push(sample(msg));
                    }
                    continue;
                }
                match client
                    .delete_message_with_retries(&channel_id, &msg.id)
                    .await
                {
                    DeleteOutcome::Deleted(wait) => {
                        if !pending_pace.is_zero() {
                            tokio::time::sleep(pending_pace).await;
                        }
                        pending_pace = std::cmp::max(api::RATE_LIMIT_DELAY, wait);
                        state.deleted += 1;
                        if let Some(reg) = registry {
                            reg.update(&channel_id, |p| {
                                p.deleted = state.deleted;
                                p.failed = state.failed;
                                p.skipped = state.skipped;
                            });
                        }
                    }
                    DeleteOutcome::Skipped => {
                        state.skipped += 1;
                        if let Some(reg) = registry {
                            reg.update(&channel_id, |p| {
                                p.skipped = state.skipped;
                                p.deleted = state.deleted;
                                p.failed = state.failed;
                            });
                        }
                    }
                    DeleteOutcome::Failed => {
                        state.failed += 1;
                        if let Some(reg) = registry {
                            reg.update(&channel_id, |p| {
                                p.failed = state.failed;
                                p.deleted = state.deleted;
                                p.skipped = state.skipped;
                            });
                        }
                    }
                    other => {
                        aborted = Some(format!("{other:?}"));
                        return Ok(());
                    }
                }
                if !dry_run {
                    save_session(&channel_id, &state, root);
                }
            }
            cursor = page.last().map(|m| m.id.clone());
            if page.len() < 100 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    .await;

    if let Err(e) = result {
        aborted = Some(e);
    }

    if aborted.is_none() {
        clear_session(&channel_id, root);
    } else {
        save_session(&channel_id, &state, root);
    }

    let elapsed = start_clock.elapsed();
    tracing::info!(
        channel = %channel_id,
        label = %label,
        scanned = state.scanned,
        deleted = state.deleted,
        failed = state.failed,
        skipped = state.skipped,
        dry_run,
        complete = aborted.is_none(),
        elapsed_s = elapsed.as_secs(),
        "deletion run finished"
    );

    Summary {
        channel_id,
        scanned: state.scanned,
        deleted: state.deleted,
        failed: state.failed,
        skipped: state.skipped,
        complete: aborted.is_none(),
        aborted,
        samples,
    }
}

/// One-shot CLI scan: report what a deletion run with the given retention
/// would delete, without deleting (always a dry run).
pub async fn scan(
    config_arg: Option<&std::path::PathBuf>,
    channel: &str,
    retention_days: Option<u32>,
) -> anyhow::Result<()> {
    let (cfg, _path) = crate::load_config(config_arg)?;
    let mut client = Client::new(cfg.token()?)?;
    if client.get_current_user().await?.is_none() {
        anyhow::bail!("invalid token or authentication failed");
    }
    let cutoff = retention_days.map(|d| Utc::now() - chrono::Duration::days(d as i64));
    let _guard = deletion_lock().lock().await;
    let summary = run_deletion(&client, channel, channel, cutoff, None, true, None, None).await;
    println!(
        "dry run: {} of your messages in range would be deleted ({} scanned, {} skipped)",
        summary.deleted, summary.scanned, summary.skipped
    );
    for s in &summary.samples {
        println!("  {s}");
    }
    Ok(())
}
