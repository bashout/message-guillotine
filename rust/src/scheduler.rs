//! The scheduler: resolves the configured chats, computes each chat's next
//! fire time (daily / weekly / monthly at the configured local time), and
//! executes jobs. A job archives the chat first when configured, then runs
//! the deletion — and every deletion goes through the process-wide
//! single-run lock, so only one deletion ever runs at a time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime};

use crate::api::Client;
use crate::archiver;
use crate::config::{ChatConfig, ChatKey, Config, ScheduleKind};
use crate::deletion;

/// A chat job resolved from the config: who/what to process and how.
#[derive(Debug, Clone)]
pub struct ChatJob {
    pub key: ChatKey,
    pub settings: ChatConfig,
}

/// Resolve the run scope: every configured guild channel, plus every DM user
/// in scope under the dm policy (nominated users, or all except the excluded
/// ones), each with its own [chat] entry or the dm defaults.
pub async fn resolve_jobs(client: &Client, cfg: &Config) -> anyhow::Result<Vec<ChatJob>> {
    let mut jobs = Vec::new();

    for (key, settings) in cfg.chats() {
        match &key {
            ChatKey::GuildChannel { .. } => jobs.push(ChatJob {
                key,
                settings: settings.clone(),
            }),
            ChatKey::Dm(user) => {
                if cfg.dm_user_in_scope(user) {
                    jobs.push(ChatJob {
                        key,
                        settings: settings.clone(),
                    });
                }
            }
        }
    }

    // DM users in scope without their own [chat] entry use the dm defaults
    // (only reachable under the exclude policy).
    let dm_channels = client.get_dm_channels().await?;
    for (user, _channel_id) in client.dm_users(&dm_channels) {
        if cfg.dm_user_in_scope(&user.id) {
            if let Some(settings) = cfg.dm_chat_settings(&user.id) {
                if !cfg.chat.contains_key(&format!("dm:{}", user.id)) {
                    jobs.push(ChatJob {
                        key: ChatKey::Dm(user.id),
                        settings,
                    });
                }
            }
        }
    }

    Ok(jobs)
}

/// Resolve a chat key to an API channel id (DM user -> DM channel).
pub async fn resolve_channel_id(client: &Client, key: &ChatKey) -> anyhow::Result<String> {
    match key {
        ChatKey::Dm(user) => client
            .dm_channel_for_user(user)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no DM channel found with user {user}")),
        ChatKey::GuildChannel { channel, .. } => Ok(channel.clone()),
    }
}

/// Process-wide cache of chat key -> friendly label, so repeated job runs
/// don't re-hit the API for the same DM user or guild channel names.
fn label_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a chat key to a friendly label: "Display (@username)" for a DM,
/// "GuildName #channel" for a guild channel. Falls back to the raw key when
/// the API lookup fails (offline, unknown id), and caches the result.
async fn chat_label(client: &Client, key: &ChatKey) -> String {
    let cache_key = key.to_string();
    if let Some(label) = label_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_key)
    {
        return label.clone();
    }
    let label = match key {
        ChatKey::Dm(user) => {
            let mut label = cache_key.clone();
            if let Ok(channels) = client.get_dm_channels().await {
                if let Some((u, _)) = client.dm_users(&channels).into_iter().find(|(u, _)| &u.id == user) {
                    let name = u.global_name.clone().unwrap_or_else(|| u.username.clone());
                    label = format!("{name} (@{})", u.username);
                }
            }
            label
        }
        ChatKey::GuildChannel { guild, channel } => {
            let mut guild_name = guild.clone();
            if let Ok(guilds) = client.get_guilds().await {
                if let Some(g) = guilds.iter().find(|g| &g.id == guild) {
                    guild_name = g.name.clone();
                }
            }
            let mut channel_name = format!("#{channel}");
            if let Ok(channels) = client.get_guild_channels(guild).await {
                if let Some(c) = channels.iter().find(|c| &c.id == channel) {
                    channel_name = format!("#{}", c.name);
                }
            }
            format!("{guild_name} {channel_name}")
        }
    };
    label_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cache_key, label.clone());
    label
}

/// Execute one chat's job: archive first when configured (appending to the
/// existing archive for this chat), then delete your messages older than the
/// retention window. The deletion phase holds the global single-run lock.
pub async fn run_chat_job(
    client: &Client,
    cfg: &Config,
    job: &ChatJob,
    registry: &crate::progress::SharedRegistry,
) -> anyhow::Result<deletion::Summary> {
    let channel_id = resolve_channel_id(client, &job.key).await?;
    let label = job.key.to_string();
    let archive_dir = cfg.resolve_archive_dir(None);

    let chat_key = job.key.to_string();
    let friendly = chat_label(client, &job.key).await;
    registry.start(&chat_key, &friendly, "starting");
    // One job at a time, archive phase included: two concurrent archives of
    // the same chat would both append the same messages (duplicate records).
    let _guard = deletion::deletion_lock().lock().await;
    if job.settings.archive_before_delete {
        tracing::info!(chat = %label, "archiving before deletion");
        registry.set_phase(&chat_key, "archiving", "writing archive (appends to existing)");
        if let Err(e) = archiver::archive_chat(client, &channel_id, &label, &archive_dir).await {
            registry.finish(&chat_key, &format!("archive failed: {e:#}"));
            return Err(e);
        }
    }

    let cutoff = chrono::Utc::now() - Duration::days(job.settings.retention_days as i64);
    let resume = deletion::load_session(&channel_id, None);
    tracing::info!(chat = %label, retention_days = job.settings.retention_days, "deletion run starting");
    registry.set_phase(&chat_key, "deleting", &format!("keeping last {} days", job.settings.retention_days));
    let summary = deletion::run_deletion(
        client,
        &channel_id,
        deletion::DeletionOptions {
            label: &label,
            cutoff: Some(cutoff),
            resume,
            dry_run: false,
            root: None,
            registry: Some(registry),
        },
    )
    .await;
    registry.finish(
        &chat_key,
        &format!("deleted {} (failed {}, skipped {})", summary.deleted, summary.failed, summary.skipped),
    );
    Ok(summary)
}

/// The next local fire time for a schedule, strictly after `from`. For
/// weekly schedules, `anchor` pins the weekday (set on first fire).
pub fn next_fire(
    schedule: &crate::config::Schedule,
    from: DateTime<Local>,
    anchor: Option<&chrono::Weekday>,
) -> DateTime<Local> {
    let at: NaiveTime = schedule.at;
    let candidate = |d: chrono::NaiveDate| {
        d.and_time(at)
            .and_local_timezone(Local)
            .single()
            .unwrap_or(from)
    };
    match schedule.kind {
        ScheduleKind::Daily => {
            let today = candidate(from.date_naive());
            if today > from {
                today
            } else {
                candidate(from.date_naive() + Duration::days(1))
            }
        }
        ScheduleKind::Weekly => {
            let target = anchor.copied().unwrap_or_else(|| from.weekday());
            let mut day = from.date_naive();
            loop {
                if day.weekday() == target {
                    let t = candidate(day);
                    if t > from {
                        return t;
                    }
                }
                day += Duration::days(1);
            }
        }
        ScheduleKind::Monthly => {
            let mut year = from.year();
            let mut month = from.month();
            loop {
                let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap();
                let t = candidate(first);
                if t > from {
                    return t;
                }
                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
            }
        }
    }
}

/// `guillotine run-now <chat>`: run one configured chat immediately. Still
/// serialized with any scheduled run via the deletion lock.
pub async fn run_now(config_arg: Option<&PathBuf>, chat: &str) -> anyhow::Result<()> {
    // When the service daemon is running, go through it: the single-deletion
    // lock only holds within one process, and the daemon is the one that
    // owns scheduled runs.
    if let Ok(resp) = crate::ipc::send(serde_json::json!({ "cmd": "run_now", "chat": chat })).await
    {
        if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            let s = &resp["summary"];
            println!(
                "run-now {chat}: deleted {}, failed {}, skipped {} ({} scanned, complete={})",
                s["deleted"], s["failed"], s["skipped"], s["scanned"], s["complete"]
            );
            return Ok(());
        } else {
            anyhow::bail!(
                "daemon refused run-now: {}",
                resp.get("error")
                    .map_or("unknown", |e| e.as_str().unwrap_or("unknown"))
            );
        }
    }
    let (cfg, _path) = crate::load_config(config_arg)?;
    let key = ChatKey::parse(chat)?;
    let mut client = Client::new(cfg.token()?)?;
    if client.get_current_user().await?.is_none() {
        anyhow::bail!("invalid token or authentication failed");
    }
    let registry = crate::progress::new_registry();
    let settings = match &key {
        ChatKey::Dm(user) => cfg
            .dm_chat_settings(user)
            .ok_or_else(|| anyhow::anyhow!("dm user {user} is not in scope under the dm policy"))?,
        ChatKey::GuildChannel { .. } => cfg
            .chat
            .get(&key.to_string())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("chat {key} has no [chat] entry in the config"))?,
    };
    let job = ChatJob { key, settings };
    let summary = run_chat_job(&client, &cfg, &job, &registry).await?;
    println!(
        "run-now {}: deleted {}, failed {}, skipped {} ({} scanned, complete={})",
        job.key,
        summary.deleted,
        summary.failed,
        summary.skipped,
        summary.scanned,
        summary.complete
    );
    Ok(())
}

/// The service loop: resolve jobs, sleep until each chat's next fire time,
/// run due jobs one at a time, and recompute. Runs forever.
pub async fn run_loop(cfg: &Config, config_path: &std::path::Path) -> anyhow::Result<()> {
    let mut client = Client::new(cfg.token()?)?;
    if client.get_current_user().await?.is_none() {
        anyhow::bail!("invalid token or authentication failed");
    }
    let registry = crate::progress::new_registry();
    tokio::spawn(crate::ipc::serve(
        std::sync::Arc::new(cfg.clone()),
        registry.clone(),
        config_path.to_path_buf(),
    ));
    // Re-resolve the DM scope periodically: DMs come and go.
    let mut jobs = resolve_jobs(&client, cfg).await?;
    // Warn about jobs whose last run is older than their schedule interval:
    // a fire missed while the service was down is not backfilled (retention is
    // relative to now, so this is safe) but it should be visible.
    {
        let last = crate::ipc::read_last_runs_public();
        for job in &jobs {
            let interval_days = match job.settings.schedule.kind {
                ScheduleKind::Daily => 1,
                ScheduleKind::Weekly => 7,
                ScheduleKind::Monthly => 31,
            };
            if let Some(run) = last.get(&job.key.to_string()) {
                if let Some(ts) = run.get("last_run").and_then(|v| v.as_str()) {
                    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) {
                        let age = chrono::Utc::now() - t.with_timezone(&chrono::Utc);
                        if age.num_days() > interval_days {
                            tracing::warn!(chat = %job.key, last_run = %ts, "scheduled run overdue (service was down?); it will fire at the next scheduled time or via run-now");
                        }
                    }
                }
            }
        }
    }
    tracing::info!(count = jobs.len(), "scheduler started");
    let mut next: Vec<(DateTime<Local>, usize)> = jobs
        .iter()
        .enumerate()
        .map(|(i, j)| (next_fire(&j.settings.schedule, Local::now(), None), i))
        .collect();

    let anchors: std::collections::HashMap<String, chrono::Weekday> = std::collections::HashMap::new();
    loop {
        // Pick up config edits (GUI save, manual edits) without a restart —
        // and re-resolve the job list so removed chats stop firing immediately.
        let cfg = match Config::load(config_path) {
            Ok(fresh) => fresh,
            Err(e) => {
                tracing::warn!(error = %e, "config reload failed; keeping last good config");
                cfg.clone()
            }
        };
        match resolve_jobs(&client, &cfg).await {
            Ok(fresh_jobs) => {
                let fresh_keys: std::collections::HashSet<String> =
                    fresh_jobs.iter().map(|j| j.key.to_string()).collect();
                for gone in jobs.iter().filter(|j| !fresh_keys.contains(&j.key.to_string())) {
                    tracing::info!(chat = %gone.key, "chat removed from config; no longer scheduled");
                }
                jobs = fresh_jobs;
                next = jobs
                    .iter()
                    .enumerate()
                    .map(|(i, j)| (next_fire(&j.settings.schedule, Local::now(), anchors.get(&j.key.to_string())), i))
                    .collect();
            }
            Err(e) => tracing::warn!(error = %e, "job re-resolution failed; keeping previous jobs"),
        }
        let cfg = &cfg;
        let now = Local::now();
        next.sort_by_key(|(t, _)| *t);
        let Some((due, idx)) = next.first().copied() else {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            continue;
        };
        if due > now {
            let wait = (due - now)
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(1));
            tokio::time::sleep(wait.min(std::time::Duration::from_secs(3600))).await;
            // Re-resolve scope hourly while waiting so DM changes are picked up.
            if wait > std::time::Duration::from_secs(3600) {
                if let Ok(fresh) = resolve_jobs(&client, cfg).await {
                    jobs = fresh;
                    next = jobs
                        .iter()
                        .enumerate()
                        .map(|(i, j)| (next_fire(&j.settings.schedule, Local::now(), None), i))
                        .collect();
                }
            }
            continue;
        }
        let job = jobs[idx].clone();
        tracing::info!(chat = %job.key, "scheduled run firing");
        match run_chat_job(&client, cfg, &job, &registry).await {
            Ok(summary) => {
                tracing::info!(chat = %job.key, deleted = summary.deleted, complete = summary.complete, "job done");
                crate::ipc::record_last_run(&job.key.to_string(), &summary);
            }
            Err(e) => tracing::error!(chat = %job.key, error = %e, "job failed"),
        }
        next[idx] = (next_fire(&job.settings.schedule, Local::now(), anchors.get(&job.key.to_string())), idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Timelike};

    fn at(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn next_fire_daily() {
        let s = crate::config::Schedule {
            kind: ScheduleKind::Daily,
            at: at(4, 0),
        };
        let from = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 3, 10)
                    .unwrap()
                    .and_hms_opt(3, 0, 0)
                    .unwrap(),
            )
            .unwrap();
        let next = next_fire(&s, from, None);
        assert_eq!((next.hour(), next.minute()), (4, 0));
        assert_eq!(next.day(), 10);
        let from = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 3, 10)
                    .unwrap()
                    .and_hms_opt(5, 0, 0)
                    .unwrap(),
            )
            .unwrap();
        let next = next_fire(&s, from, None);
        assert_eq!(next.day(), 11);
    }

    #[test]
    fn next_fire_monthly_is_first_of_month() {
        let s = crate::config::Schedule {
            kind: ScheduleKind::Monthly,
            at: at(4, 0),
        };
        let from = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 1, 15)
                    .unwrap()
                    .and_hms_opt(12, 0, 0)
                    .unwrap(),
            )
            .unwrap();
        let next = next_fire(&s, from, None);
        assert_eq!((next.month(), next.day()), (2, 1));
        let from = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 12, 31)
                    .unwrap()
                    .and_hms_opt(23, 0, 0)
                    .unwrap(),
            )
            .unwrap();
        let next = next_fire(&s, from, None);
        assert_eq!((next.year(), next.month(), next.day()), (2025, 1, 1));
    }

    #[test]
    fn next_fire_weekly_is_within_seven_days() {
        let s = crate::config::Schedule {
            kind: ScheduleKind::Weekly,
            at: at(4, 0),
        };
        let from = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 3, 10)
                    .unwrap()
                    .and_hms_opt(3, 0, 0)
                    .unwrap(),
            )
            .unwrap();
        let next = next_fire(&s, from, None);
        let delta = (next - from).num_seconds();
        assert!(delta <= 7 * 86_400 && delta > 0);
    }
}
