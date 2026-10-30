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

/// Atomically rewrite meta.json with owner-only permissions (temp file
/// created 0600 + rename), like the manifest.
pub fn write_meta(folder: &Path, meta: &serde_json::Value) {
    let path = folder.join("meta.json");
    let _ = crate::config::write_file_private_atomic(
        &path,
        &(serde_json::to_string(meta).unwrap() + "\n"),
    );
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
        let mtime = match std::fs::metadata(&marker)
            .or_else(|_| std::fs::metadata(&path))
            .and_then(|m| m.modified())
        {
            Ok(t) => t,
            Err(_) => continue, // unreadable candidate: skip, don't abort the search
        };
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

pub fn mark_archive_status(folder: &Path, complete: bool) {
    let path = folder.join("status.json");
    let _ = crate::config::write_file_private_atomic(
        &path,
        &(serde_json::to_string(&serde_json::json!({ "complete": complete })).unwrap() + "\n"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Message;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "guillotine-af-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn mk_archive(root: &Path, name: &str, meta: serde_json::Value) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
        dir
    }

    #[test]
    fn load_message_records_truncates_partial_tail() {
        let root = temp_root("tail");
        let path = root.join("messages.jsonl");
        let mut body = String::new();
        body.push_str("{\"id\": \"100\", \"content\": \"a\"}\n");
        body.push_str("{\"id\": \"101\", \"content\": \"b\"}\n");
        body.push_str("{\"id\": \"10"); // hard-kill partial tail, no newline
        std::fs::write(&path, &body).unwrap();
        let records = load_message_records(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["id"], "100");
        assert_eq!(records[1]["id"], "101");
        // the file itself is truncated back to the last valid line
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "{\"id\": \"100\", \"content\": \"a\"}\n{\"id\": \"101\", \"content\": \"b\"}\n");
        // loading again is a no-op and still yields both records
        assert_eq!(load_message_records(&path).len(), 2);
        // a fully valid file is left untouched
        std::fs::write(&path, "{\"id\": \"1\"}\n").unwrap();
        assert_eq!(load_message_records(&path).len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"id\": \"1\"}\n");
        // missing file -> empty
        assert!(load_message_records(&root.join("nope.jsonl")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_existing_archive_recency_and_merged_matching() {
        let root = temp_root("find");
        let older = mk_archive(&root, "a-old", serde_json::json!({"channel_id": "111", "chat": "old"}));
        std::fs::write(older.join("messages.jsonl"), "{\"id\": \"1\"}\n").unwrap();
        // merged archive listing the channel among channel_ids
        let merged = mk_archive(
            &root,
            "b-merged",
            serde_json::json!({"channel_id": "222", "channel_ids": ["222", "111"], "chat": "merged"}),
        );
        std::fs::write(merged.join("messages.jsonl"), "{\"id\": \"2\"}\n").unwrap();
        // unrelated archive
        let other = mk_archive(&root, "c-other", serde_json::json!({"channel_id": "333"}));
        std::fs::write(other.join("messages.jsonl"), "{\"id\": \"3\"}\n").unwrap();

        // make "a-old" older than "b-merged" by mtime
        let old_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let new_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2000);
        std::fs::File::options()
            .write(true)
            .open(older.join("messages.jsonl"))
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(merged.join("messages.jsonl"))
            .unwrap()
            .set_modified(new_time)
            .unwrap();

        // merged archive matches channel 111 via channel_ids and is most recent
        let found = find_existing_archive("111", &root).unwrap();
        assert_eq!(found, merged);
        // channel 333 only in the unrelated archive
        assert_eq!(find_existing_archive("333", &root).unwrap(), other);
        // unknown channel -> None
        assert!(find_existing_archive("999", &root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn minimal_message_projection_keys() {
        let msg: Message = serde_json::from_value(serde_json::json!({
            "id": "123",
            "timestamp": "2024-01-01T00:00:00.000+00:00",
            "author": {"id": "u1", "username": "alice", "global_name": "Alice A."},
            "content": "hi",
            "type": 0,
            "attachments": [{"id": "a1", "filename": "f.png", "url": "https://x/f.png"}],
            "pinned": true,
            "edited_timestamp": "2024-01-02T00:00:00.000+00:00",
            "message_reference": {"message_id": "100"}
        }))
        .unwrap();
        let m = minimal_message(&msg);
        let obj = m.as_object().unwrap();
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["attachment_ids", "author", "author_id", "content", "edited_timestamp", "id", "pinned", "reply_to", "timestamp", "type"]
        );
        assert_eq!(m["author"], "Alice A.");
        assert_eq!(m["reply_to"], "100");
        assert_eq!(m["attachment_ids"], serde_json::json!(["a1"]));
        // global_name absent -> username; no reference -> null
        let msg: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "timestamp": "2024-01-01T00:00:00.000+00:00",
            "author": {"id": "u2", "username": "bob"}
        }))
        .unwrap();
        let m = minimal_message(&msg);
        assert_eq!(m["author"], "bob");
        assert!(m["reply_to"].is_null());
        assert_eq!(m["attachment_ids"].as_array().unwrap().len(), 0);
        assert_eq!(m["type"], 0);
        assert_eq!(m["pinned"], false);
    }
}
