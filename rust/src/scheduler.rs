//! The scheduler: resolves the configured chats, computes each chat's next
//! fire time (daily / weekly / monthly at the configured local time), and
//! executes jobs. A job archives the chat first when configured, then runs
//! the deletion — and every deletion goes through the process-wide
//! single-run lock, so only one deletion ever runs at a time.

use std::path::PathBuf;

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, Timelike};

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
    registry.start(&chat_key, "starting");
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
        &label,
        Some(cutoff),
        resume,
        false,
        None,
        Some(registry),
    )
    .await;
    registry.finish(
        &chat_key,
        &format!("deleted {} (failed {}, skipped {})", summary.deleted, summary.failed, summary.skipped),
    );
    Ok(summary)
}

/// The next local fire time for a schedule, strictly after `from`.
pub fn next_fire(schedule: &crate::config::Schedule, from: DateTime<Local>) -> DateTime<Local> {
    let at: NaiveTime = schedule.at;
    let candidate = |d: chrono::NaiveDate| {
        d.and_time(at)
            .and_local_timezone(Local)
            .single()
            .unwrap_or_else(|| from)
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
            let mut day = from.date_naive();
            loop {
                let t = candidate(day);
                if t > from {
                    return t;
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
    tokio::spawn(crate::ipc::serve(std::sync::Arc::new(cfg.clone()), registry.clone()));
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
pub async fn run_loop(cfg: &Config) -> anyhow::Result<()> {
    let mut client = Client::new(cfg.token()?)?;
    if client.get_current_user().await?.is_none() {
        anyhow::bail!("invalid token or authentication failed");
    }
    let registry = crate::progress::new_registry();
    tokio::spawn(crate::ipc::serve(std::sync::Arc::new(cfg.clone()), registry.clone()));
    // Re-resolve the DM scope periodically: DMs come and go.
    let mut jobs = resolve_jobs(&client, cfg).await?;
    tracing::info!(count = jobs.len(), "scheduler started");
    let mut next: Vec<(DateTime<Local>, usize)> = jobs
        .iter()
        .enumerate()
        .map(|(i, j)| (next_fire(&j.settings.schedule, Local::now()), i))
        .collect();

    loop {
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
                        .map(|(i, j)| (next_fire(&j.settings.schedule, Local::now()), i))
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
        next[idx] = (next_fire(&job.settings.schedule, Local::now()), idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};

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
        let next = next_fire(&s, from);
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
        let next = next_fire(&s, from);
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
        let next = next_fire(&s, from);
        assert_eq!((next.month(), next.day()), (2, 1));
        let from = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 12, 31)
                    .unwrap()
                    .and_hms_opt(23, 0, 0)
                    .unwrap(),
            )
            .unwrap();
        let next = next_fire(&s, from);
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
        let next = next_fire(&s, from);
        let delta = (next - from).num_seconds();
        assert!(delta <= 7 * 86_400 && delta > 0);
    }
}
