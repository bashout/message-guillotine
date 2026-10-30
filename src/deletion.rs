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
    /// Kept for on-disk format parity with V1 session files.
    #[allow(dead_code)]
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
        let _ = config::ensure_data_dir();
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.part");
    if config::write_file_private(&tmp, &(serde_json::to_string(state).unwrap() + "\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub fn clear_session(channel_id: &str, root: Option<&Path>) {
    let _ = std::fs::remove_file(session_path(channel_id, root));
}

#[derive(Debug, Clone)]
pub struct Summary {
    #[allow(dead_code)] // part of the run report; kept for logging/IPC parity
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
    let content = if content.chars().count() > 60 {
        format!("{}...", content.chars().take(57).collect::<String>())
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
/// The session state a run starts from: resume counters/cursor carry over
/// when resuming; otherwise a fresh state whose scan jumps straight to the
/// cutoff boundary (`before` the cutoff's snowflake, +1ms to keep the bound
/// inclusive — the API's `before` is exclusive, so a message exactly at the
/// cutoff must have an id strictly below the bound).
fn initial_session(
    channel_id: &str,
    label: &str,
    cutoff: Option<DateTime<Utc>>,
    resume: Option<Session>,
) -> Session {
    let mut state = Session {
        channel_id: channel_id.to_string(),
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
        let bound = cutoff + chrono::Duration::milliseconds(1);
        state.before_cursor = Some(api::snowflake_for(bound));
    }
    state
}

/// How one message's timestamp compares to the cutoff. Fail safe: an
/// undatable timestamp is never treated as old enough to delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutoffCheck {
    /// Newer than the cutoff: out of range, skip silently.
    Newer,
    /// In range: eligible for deletion.
    InRange,
    /// Undatable timestamp: skip and count as failed, never delete.
    Undatable,
}

fn cutoff_check(epoch: &anyhow::Result<f64>, cutoff: Option<f64>) -> CutoffCheck {
    let Some(cut) = cutoff else {
        return CutoffCheck::InRange;
    };
    match epoch {
        Ok(ts) if *ts > cut => CutoffCheck::Newer,
        Ok(_) => CutoffCheck::InRange,
        Err(_) => CutoffCheck::Undatable,
    }
}

/// Persist or drop the session file at the end of a run. A dry run never
/// writes and never clears; an aborted run saves (resume); a complete run
/// clears.
fn persist_session(channel_id: &str, state: &Session, aborted: bool, dry_run: bool, root: Option<&Path>) {
    if aborted && !dry_run {
        save_session(channel_id, state, root);
    } else if !aborted && !dry_run {
        clear_session(channel_id, root);
    }
}

/// Periodically persist the session mid-run (every 25 scanned messages) so
/// a killed run can resume; never during a dry run.
fn maybe_save_session(channel_id: &str, state: &Session, dry_run: bool, root: Option<&Path>) {
    if !dry_run && state.scanned.is_multiple_of(25) {
        save_session(channel_id, state, root);
    }
}

/// Mirror the run counters into the live job registry, if one is attached.
fn update_registry(
    registry: Option<&crate::progress::SharedRegistry>,
    label: &str,
    state: &Session,
) {
    if let Some(reg) = registry {
        reg.update(label, |p| {
            p.scanned = state.scanned;
            p.deleted = state.deleted;
            p.failed = state.failed;
            p.skipped = state.skipped;
        });
    }
}

/// Everything a deletion run needs beyond the client and channel.
pub struct DeletionOptions<'a> {
    pub label: &'a str,
    pub cutoff: Option<DateTime<Utc>>,
    pub resume: Option<Session>,
    pub dry_run: bool,
    pub root: Option<&'a Path>,
    pub registry: Option<&'a crate::progress::SharedRegistry>,
}

pub async fn run_deletion(
    client: &Client,
    channel_id: &str,
    opts: DeletionOptions<'_>,
) -> Summary {
    let DeletionOptions { label, cutoff, resume, dry_run, root, registry } = opts;
    let channel_id = channel_id.to_string();
    let cutoff_ts = cutoff.map(|c| c.timestamp() as f64);

    let mut state = initial_session(&channel_id, label, cutoff, resume);

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
                    let epoch = msg.epoch();
                    match cutoff_check(&epoch, Some(cut)) {
                        CutoffCheck::Newer => continue, // newer than the cutoff
                        CutoffCheck::InRange => {}
                        CutoffCheck::Undatable => {
                            // Fail safe: never treat an undatable message as
                            // old enough to delete.
                            if let Err(e) = &epoch {
                                tracing::warn!(id = %msg.id, error = %e, "undatable message timestamp; skipping");
                            }
                            state.failed += 1;
                            continue;
                        }
                    }
                }
                state.scanned += 1;
                state.before_cursor = Some(msg.id.clone());
                update_registry(registry, label, &state);

                if Some(&msg.author.id) != me_id.as_ref() {
                    maybe_save_session(&channel_id, &state, dry_run, root);
                    continue;
                }
                if msg.is_system() {
                    state.skipped += 1;
                    maybe_save_session(&channel_id, &state, dry_run, root);
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
                        update_registry(registry, label, &state);
                    }
                    DeleteOutcome::Skipped => {
                        state.skipped += 1;
                        update_registry(registry, label, &state);
                    }
                    DeleteOutcome::Failed => {
                        state.failed += 1;
                        update_registry(registry, label, &state);
                    }
                    other => {
                        aborted = Some(format!("{other:?}"));
                        return Ok(());
                    }
                }
                maybe_save_session(&channel_id, &state, dry_run, root);
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

    persist_session(&channel_id, &state, aborted.is_some(), dry_run, root);

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
    if !config::valid_id(channel) {
        anyhow::bail!("channel id must be numeric");
    }
    let (cfg, _path) = crate::load_config(config_arg)?;
    let client = api::authed_client(&cfg).await?;
    let cutoff = retention_days.map(|d| {
        let d = d.min(36_500);
        Utc::now() - chrono::Duration::days(d as i64)
    });
    let _guard = deletion_lock().lock().await;
    let summary = run_deletion(
        &client,
        channel,
        DeletionOptions { label: channel, cutoff, resume: None, dry_run: true, root: None, registry: None },
    )
    .await;
    println!(
        "dry run: {} of your messages in range would be deleted ({} scanned, {} skipped)",
        summary.deleted, summary.scanned, summary.skipped
    );
    for s in &summary.samples {
        println!("  {s}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "guillotine-deletion-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn session(channel: &str, scanned: u64, deleted: u64, failed: u64, skipped: u64, cursor: Option<&str>, complete: bool) -> Session {
        Session {
            channel_id: channel.to_string(),
            label: "label".to_string(),
            start_time: None,
            end_time: None,
            before_cursor: cursor.map(str::to_string),
            scanned,
            deleted,
            failed,
            skipped,
            complete,
        }
    }

    #[test]
    fn session_save_load_clear_roundtrip() {
        let root = temp_root("roundtrip");
        let s = session("123", 10, 4, 1, 2, Some("999"), false);
        save_session("123", &s, Some(&root));
        let loaded = load_session("123", Some(&root)).expect("session loads");
        assert_eq!(loaded.channel_id, "123");
        assert_eq!(loaded.scanned, 10);
        assert_eq!(loaded.deleted, 4);
        assert_eq!(loaded.failed, 1);
        assert_eq!(loaded.skipped, 2);
        assert_eq!(loaded.before_cursor.as_deref(), Some("999"));
        assert!(!loaded.complete);
        // no .part file left behind (atomic rename)
        assert!(!session_path("123", Some(&root)).with_extension("json.part").exists());
        clear_session("123", Some(&root));
        assert!(load_session("123", Some(&root)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_session_file_is_absent() {
        let root = temp_root("corrupt");
        std::fs::write(session_path("42", Some(&root)), "{ not json").unwrap();
        assert!(load_session("42", Some(&root)).is_none());
        // valid JSON but not a Session (missing required fields)
        std::fs::write(session_path("43", Some(&root)), "{\"channel_id\": \"43\"}").unwrap();
        assert!(load_session("43", Some(&root)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn complete_session_is_absent() {
        let root = temp_root("complete");
        let s = session("7", 5, 5, 0, 0, None, true);
        save_session("7", &s, Some(&root));
        assert!(load_session("7", Some(&root)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cutoff_bound_is_inclusive_via_plus_one_ms() {
        let cutoff = Utc.with_ymd_and_hms(2024, 6, 15, 12, 0, 0).unwrap();
        let state = initial_session("1", "lbl", Some(cutoff), None);
        assert_eq!(state.end_time.as_deref(), Some("2024-06-15T12:00:00+00:00"));
        let bound: u128 = state.before_cursor.unwrap().parse().unwrap();
        let bound_ms = (bound >> 22) as i64 + api::DISCORD_EPOCH_MS;
        // bound = cutoff + 1ms exactly
        assert_eq!(bound_ms, cutoff.timestamp_millis() + 1);
        // a message exactly at the cutoff has id < bound => included by `before`
        let at_cutoff = api::snowflake_for(cutoff).parse::<u128>().unwrap();
        assert!(at_cutoff < bound);
        // a message 1ms after the cutoff is at the bound => excluded by `before`
        let after = api::snowflake_for(cutoff + chrono::Duration::milliseconds(1))
            .parse::<u128>()
            .unwrap();
        assert!(after >= bound);
    }

    #[test]
    fn resume_carries_counters_and_cursor() {
        let cutoff = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let resume = session("9", 100, 40, 3, 7, Some("555"), false);
        let state = initial_session("9", "lbl", Some(cutoff), Some(resume));
        assert_eq!(state.scanned, 100);
        assert_eq!(state.deleted, 40);
        assert_eq!(state.failed, 3);
        assert_eq!(state.skipped, 7);
        assert_eq!(state.before_cursor.as_deref(), Some("555"));
        assert!(!state.complete);
        // no cutoff: everything is in range, no bound cursor
        let state = initial_session("9", "lbl", None, None);
        assert!(state.before_cursor.is_none());
        assert!(state.end_time.is_none());
    }

    #[test]
    fn cutoff_check_fail_safe_on_undatable() {
        let cut = 1_700_000_000.0;
        assert_eq!(cutoff_check(&Ok(cut + 0.5), Some(cut)), CutoffCheck::Newer);
        assert_eq!(cutoff_check(&Ok(cut), Some(cut)), CutoffCheck::InRange);
        assert_eq!(cutoff_check(&Ok(cut - 1.0), Some(cut)), CutoffCheck::InRange);
        let undatable: anyhow::Result<f64> = Err(anyhow::anyhow!("bad timestamp"));
        assert_eq!(cutoff_check(&undatable, Some(cut)), CutoffCheck::Undatable);
        // no cutoff: everything in range, even undatable
        assert_eq!(cutoff_check(&undatable, None), CutoffCheck::InRange);
    }

    #[test]
    fn dry_run_never_writes_or_clears_session() {
        let root = temp_root("dryrun");
        let s = session("5", 3, 2, 0, 1, Some("111"), false);
        // dry run, aborted: no save, no clear
        persist_session("5", &s, true, true, Some(&root));
        assert!(!session_path("5", Some(&root)).exists());
        // dry run, complete: still no clear of a pre-existing file
        save_session("5", &s, Some(&root));
        persist_session("5", &s, false, true, Some(&root));
        assert!(session_path("5", Some(&root)).exists(), "dry run must not clear");
        // real run, complete: clears
        persist_session("5", &s, false, false, Some(&root));
        assert!(!session_path("5", Some(&root)).exists());
        // real run, aborted: saves with counters intact
        let s2 = session("6", 8, 5, 1, 2, Some("222"), false);
        persist_session("6", &s2, true, false, Some(&root));
        let loaded = load_session("6", Some(&root)).unwrap();
        assert_eq!((loaded.scanned, loaded.deleted, loaded.failed, loaded.skipped), (8, 5, 1, 2));
        let _ = std::fs::remove_dir_all(&root);
    }
}
