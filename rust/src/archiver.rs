//! Archive writing: stream a chat to disk in the V1-compatible format and
//! resume interrupted runs. Archives always append to an existing archive
//! folder for the same channel when one is present.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::api::{self, Client};
use crate::archive_format as fmt;

/// Strip characters that are unsafe in filenames.
pub fn sanitize_filename(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == '.' || c == '_');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Rewrite manifest.jsonl with one entry per attachment (latest wins).
/// Atomic — temp file + rename.
fn compact_manifest(folder: &Path) -> Vec<serde_json::Value> {
    let mut latest: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let path = folder.join("manifest.jsonl");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        for line in raw.lines() {
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(entry) => {
                    if let Some(id) = entry.get("attachment_id").and_then(|v| v.as_str()) {
                        latest.insert(id.to_string(), entry);
                    }
                }
                Err(_) => break,
            }
        }
    }
    let entries: Vec<serde_json::Value> = latest.values().cloned().collect();
    let tmp = folder.join("manifest.jsonl.part");
    let body: String = entries
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect();
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    entries
}

/// Rewrite messages.jsonl in message-id order (ids are chronological
/// snowflakes). Atomic; skipped when already sorted.
fn sort_message_records(folder: &Path) {
    let path = folder.join("messages.jsonl");
    let records = fmt::load_message_records(&path);
    if records.is_empty() {
        return;
    }
    let ids: Vec<u128> = records
        .iter()
        .filter_map(|r| {
            r.get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    if ids == sorted {
        return;
    }
    let mut paired: Vec<(u128, &serde_json::Value)> = records
        .iter()
        .filter_map(|r| {
            r.get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u128>().ok())
                .map(|id| (id, r))
        })
        .collect();
    paired.sort_by_key(|(id, _)| *id);
    let tmp = folder.join("messages.jsonl.part");
    let body: String = paired
        .iter()
        .map(|(_, r)| serde_json::to_string(r).unwrap() + "\n")
        .collect();
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Record channel_id as one of the channels archived in folder (a merge).
fn register_merged_channel(folder: &Path, channel_id: &str) {
    let mut meta = fmt::read_meta(folder);
    let mut ids: Vec<String> = meta
        .get("channel_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            meta.get("channel_id")
                .and_then(|c| c.as_str())
                .map(|c| vec![c.to_string()])
                .unwrap_or_default()
        });
    if !ids.iter().any(|c| c == channel_id) {
        ids.push(channel_id.to_string());
    }
    let mut cursors: BTreeMap<String, String> = meta
        .get("channel_cursors")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("0").to_string()))
                .collect()
        })
        .unwrap_or_default();
    if cursors.is_empty() && !ids.is_empty() && ids[0] != channel_id {
        if let Some(records) = std::fs::read_to_string(folder.join("messages.jsonl")).ok() {
            if let Some(last) = records.lines().last() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(last) {
                    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                        cursors.insert(ids[0].clone(), id.to_string());
                    }
                }
            }
        }
    }
    cursors
        .entry(channel_id.to_string())
        .or_insert_with(|| "0".to_string());
    meta["channel_ids"] = serde_json::json!(ids);
    meta["channel_cursors"] = serde_json::json!(cursors);
    fmt::write_meta(folder, &meta);
}

/// Advance the channel's resume cursor to last_id.
fn update_channel_cursor(folder: &Path, channel_id: &str, last_id: Option<&str>) {
    let Some(last_id) = last_id else { return };
    let mut meta = fmt::read_meta(folder);
    let mut cursors: BTreeMap<String, String> = meta
        .get("channel_cursors")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("0").to_string()))
                .collect()
        })
        .unwrap_or_default();
    if cursors.is_empty() {
        return;
    }
    cursors.insert(channel_id.to_string(), last_id.to_string());
    meta["channel_cursors"] = serde_json::json!(cursors);
    fmt::write_meta(folder, &meta);
}

/// Archive the chat as a stream: fetch forward from the beginning (or the
/// resume cursor), write each message as it arrives. An existing archive for
/// this channel is resumed/appended automatically.
pub async fn archive_chat(
    client: &Client,
    channel_id: &str,
    channel_label: &str,
    archive_dir: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let channel_id = channel_id.to_string();
    let archive_dir = archive_dir.to_path_buf();

    let (folder, mut archived_ids, mut after_id) =
        match fmt::find_existing_archive(&channel_id, &archive_dir) {
            Some(folder) => {
                let records = fmt::load_message_records(&folder.join("messages.jsonl"));
                let ids: BTreeSet<String> = records
                    .iter()
                    .filter_map(|r| r.get("id").and_then(|v| v.as_str().map(String::from)))
                    .collect();
                let cursors = fmt::read_channel_cursors(&folder);
                let after = if let Some(c) = cursors.get(&channel_id) {
                    c.clone()
                } else if !cursors.is_empty() {
                    "0".to_string()
                } else {
                    records
                        .last()
                        .and_then(|r| r.get("id").and_then(|v| v.as_str().map(String::from)))
                        .unwrap_or_else(|| "0".to_string())
                };
                tracing::info!(folder = %folder.display(), count = ids.len(), "resuming archive");
                (folder, ids, after)
            }
            None => {
                let date_part = chrono::Local::now().format("%d-%m-%y");
                let label_part = sanitize_filename(channel_label, "");
                let label_part = if label_part.is_empty() {
                    format!("chat-{channel_id}")
                } else {
                    label_part
                };
                let mut folder = archive_dir.join(format!("{label_part}-archive-{date_part}"));
                if folder.exists() {
                    let existing = fmt::read_meta(&folder);
                    if existing.get("channel_id").and_then(|c| c.as_str())
                        != Some(channel_id.as_str())
                    {
                        folder = archive_dir
                            .join(format!("{label_part}-{channel_id}-archive-{date_part}"));
                    }
                }
                std::fs::create_dir_all(&folder)?;
                fmt::write_meta(
                    &folder,
                    &serde_json::json!({ "channel_id": channel_id, "chat": channel_label }),
                );
                tracing::info!(folder = %folder.display(), "starting new archive");
                (folder, BTreeSet::new(), "0".to_string())
            }
        };

    let att_dir = folder.join("attachments");
    if att_dir.exists() {
        for stale in std::fs::read_dir(&att_dir)? {
            if let Ok(e) = stale {
                if e.file_name().to_string_lossy().ends_with(".part") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    let mut entries = compact_manifest(&folder);
    let mut recorded: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut seq = 0u64;
    for e in &entries {
        if let Some(id) = e.get("attachment_id").and_then(|v| v.as_str()) {
            recorded.insert(id.to_string(), e.clone());
        }
        if let Some(s) = e.get("seq").and_then(|v| v.as_u64()) {
            seq = seq.max(s);
        }
    }

    let mut saved = 0u64;
    let mut failed = 0u64;
    let mut last_id: Option<String> = None;
    let mut count = archived_ids.len() as u64;

    let messages_result: Result<(), anyhow::Error> = async {
        let mut cursor = after_id;
        loop {
            let page = match client.history_page_after(&channel_id, &cursor).await {
                Ok(p) => p,
                Err(e) => return Err(anyhow::anyhow!("{e}")),
            };
            if page.is_empty() {
                return Ok(());
            }
            for msg in &page {
                if archived_ids.contains(&msg.id) {
                    last_id = Some(msg.id.clone());
                    continue;
                }
                for att in &msg.attachments {
                    let already = recorded.get(&att.id).map_or(false, |e| {
                        e.get("status").and_then(|s| s.as_str()) == Some("ok")
                            && e.get("local_path").and_then(|p| p.as_str()).map_or(false, |p| {
                                folder.join(p).exists()
                            })
                    });
                    if already {
                        continue;
                    }
                    seq += 1;
                    std::fs::create_dir_all(&att_dir)?;
                    let safe_name = sanitize_filename(
                        att.filename.as_deref().unwrap_or(""),
                        &format!("att-{}", att.id),
                    );
                    let dest = att_dir.join(format!("{seq:04}_{}_{safe_name}", msg.id));
                    let size = match &att.url {
                        Some(url) => client.download_attachment(url, &dest).await,
                        None => None,
                    };
                    let entry = serde_json::json!({
                        "seq": seq,
                        "message_id": msg.id,
                        "attachment_id": att.id,
                        "filename": att.filename,
                        "url": att.url,
                        "local_path": if size.is_some() { Some(format!("attachments/{}", dest.file_name().unwrap().to_string_lossy())) } else { None::<String> },
                        "size": size,
                        "status": if size.is_some() { "ok" } else { "failed" },
                    });
                    {
                        let mut mf = std::fs::OpenOptions::new()
                            .append(true)
                            .open(folder.join("manifest.jsonl"))?;
                        use std::io::Write;
                        mf.write_all((serde_json::to_string(&entry).unwrap() + "\n").as_bytes())?;
                    }
                    if size.is_some() {
                        saved += 1;
                    } else {
                        failed += 1;
                    }
                    recorded.insert(att.id.clone(), entry);
                    tokio::time::sleep(api::ATTACHMENT_DELAY).await;
                }
                {
                    let mut jf = std::fs::OpenOptions::new()
                        .append(true)
                        .open(folder.join("messages.jsonl"))?;
                    use std::io::Write;
                    jf.write_all((serde_json::to_string(&fmt::minimal_message(msg)).unwrap() + "\n").as_bytes())?;
                }
                archived_ids.insert(msg.id.clone());
                last_id = Some(msg.id.clone());
                count = archived_ids.len() as u64;
                tracing::info!(count, saved, failed, "archiving");
            }
            cursor = page.last().map(|m| m.id.clone()).unwrap();
            if page.len() < 100 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    .await;

    // Whatever happened, leave the folder consistent and resumable.
    sort_message_records(&folder);
    update_channel_cursor(&folder, &channel_id, last_id.as_deref());
    match &messages_result {
        Ok(()) => {
            compact_manifest(&folder);
            fmt::mark_archive_status(&folder, true);
            tracing::info!(folder = %folder.display(), count, "archive complete");
        }
        Err(e) => {
            compact_manifest(&folder);
            fmt::mark_archive_status(&folder, false);
            tracing::warn!(folder = %folder.display(), error = %e, "archive incomplete; rerun to resume");
        }
    }
    match messages_result {
        Ok(()) => Ok(Some(folder)),
        Err(e) => Err(anyhow::anyhow!("archive incomplete: {e}; rerun to resume")),
    }
}

use std::time::Duration;

/// One-shot CLI archive of a channel.
pub async fn archive(config_arg: Option<&std::path::PathBuf>, channel: &str) -> anyhow::Result<()> {
    let (cfg, _path) = crate::load_config(config_arg)?;
    let mut client = Client::new(cfg.token()?)?;
    if client.get_current_user().await?.is_none() {
        anyhow::bail!("invalid token or authentication failed");
    }
    let archive_dir = cfg.resolve_archive_dir(None);
    let folder = archive_chat(&client, channel, channel, &archive_dir).await?;
    if let Some(folder) = folder {
        println!("Archived to {}", folder.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_rules() {
        assert_eq!(
            sanitize_filename("hello world.txt", "file"),
            "hello_world.txt"
        );
        assert_eq!(sanitize_filename("emoji🔥name", "file"), "emoji_name");
        assert_eq!(sanitize_filename("🔥🔥🔥", "att-1"), "att-1");
        assert_eq!(sanitize_filename("..hidden..", "file"), "hidden");
    }
}
