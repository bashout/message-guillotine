//! On-disk archive format shared by the archiver (writer) and the Python
//! viewer (reader). Kept byte-compatible with V1:
//!
//!     meta.json          channel id + chat name (+ channel_ids/cursors when merged)
//!     messages.jsonl     one message record per line
//!     manifest.jsonl     one attachment record per line
//!     attachments/       downloaded attachment files
//!     status.json        completion flag from the last run

use std::path::{Path, PathBuf};

use crate::api::Message;

/// Project a raw API message down to the minimal archival record (same keys
/// as V1's minimal_message).
pub fn minimal_message(msg: &Message) -> serde_json::Value {
    serde_json::json!({
        "id": msg.id,
        "author_id": msg.author.id,
        "author": msg.author.global_name.clone().unwrap_or_else(|| msg.author.username.clone()),
        "timestamp": msg.timestamp,
        "edited_timestamp": msg.edited_timestamp,
        "pinned": msg.pinned,
        "content": msg.content,
        "type": msg.r#type,
        "reply_to": msg.message_reference.as_ref().and_then(|r| r.message_id.clone()),
        "attachment_ids": msg.attachments.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
    })
}

pub fn read_meta(folder: &Path) -> serde_json::Value {
    std::fs::read_to_string(folder.join("meta.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::json!({}))
}

/// Atomically rewrite meta.json (temp file + rename), like the manifest.
pub fn write_meta(folder: &Path, meta: &serde_json::Value) {
    let path = folder.join("meta.json");
    let tmp = folder.join("meta.json.part");
    if std::fs::write(&tmp, serde_json::to_string(meta).unwrap() + "\n").is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Per-channel resume cursors of a merged archive: channel id -> id of the
/// newest message archived for that channel.
pub fn read_channel_cursors(folder: &Path) -> std::collections::BTreeMap<String, String> {
    read_meta(folder)
        .get("channel_cursors")
        .and_then(|c| c.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("0").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// All archive folders directly under archive_dir (a meta.json marks an
/// archive), sorted by name.
pub fn list_archives(archive_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(children) = std::fs::read_dir(archive_dir) {
        for entry in children.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("meta.json").is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Locate the most recent archive of this channel in the archival folder
/// (resume target). A merged archive matches when the channel is any of its
/// recorded channels. Recency is judged by messages.jsonl mtime.
pub fn find_existing_archive(channel_id: &str, archive_dir: &Path) -> Option<PathBuf> {
    let mut matches: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for path in list_archives(archive_dir) {
        let meta = read_meta(&path);
        let mut channels: Vec<String> = Vec::new();
        if let Some(c) = meta.get("channel_id").and_then(|c| c.as_str()) {
            channels.push(c.to_string());
        }
        if let Some(list) = meta.get("channel_ids").and_then(|c| c.as_array()) {
            channels.extend(list.iter().filter_map(|c| c.as_str().map(String::from)));
        }
        if !channels.iter().any(|c| c == channel_id) {
            continue;
        }
        let marker = path.join("messages.jsonl");
        let mtime = std::fs::metadata(&marker)
            .or_else(|_| std::fs::metadata(&path))
            .and_then(|m| m.modified())
            .ok()?;
        matches.push((mtime, path));
    }
    matches.sort_by_key(|(t, _)| *t);
    matches.pop().map(|(_, p)| p)
}

/// Load message records from messages.jsonl, truncating any partial tail
/// line left behind by a hard kill (a partial line is always the last).
pub fn load_message_records(path: &Path) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    let Ok(bytes) = std::fs::read(path) else {
        return records;
    };
    let mut valid_end = 0usize;
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            valid_end += 1;
            continue;
        }
        match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(v) => {
                records.push(v);
                valid_end += line.len() + 1;
            }
            Err(_) => break,
        }
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if valid_end < meta.len() as usize {
            let _ = std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|mut f| {
                    use std::io::Seek;
                    f.seek(std::io::SeekFrom::Start(valid_end as u64))?;
                    f.set_len(valid_end as u64)
                });
        }
    }
    records
}

/// The completion flag recorded by the last run: Some(true) finished,
/// Some(false) interrupted/failed, None when never written.
pub fn read_archive_status(folder: &Path) -> Option<bool> {
    std::fs::read_to_string(folder.join("status.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("complete").and_then(|c| c.as_bool()))
}

pub fn mark_archive_status(folder: &Path, complete: bool) {
    let _ = std::fs::write(
        folder.join("status.json"),
        serde_json::to_string(&serde_json::json!({ "complete": complete })).unwrap() + "\n",
    );
}
