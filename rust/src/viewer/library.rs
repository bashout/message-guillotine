//! Archive reading: the read-only loaded view of one archive folder
//! (`Archive`), the archival-folder scanner (`find_archives`), and the
//! library (root folder + small LRU of loaded archives + selection).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::{Filters, LIBRARY_CACHE_MAX, VIEW_CACHE_MAX};
use crate::archive_format::read_meta;

// ---------------------------------------------------------------------------
// Archive (read-only loaded view of one archive folder)
// ---------------------------------------------------------------------------

struct AttachmentRecord {
    entry: Value,
    year: String,
    date: Option<String>,
}

pub struct Archive {
    folder: PathBuf,
    meta: Value,
    pub(crate) messages: Vec<Value>,
    index_by_id: HashMap<String, usize>,
    attachments_by_id: HashMap<String, Value>,
    /// Manifest entries with a local file, in seq order.
    attachments: Vec<Value>,
    attachment_records: Vec<AttachmentRecord>,
    attachment_indices: std::collections::HashSet<usize>,
    link_indices: std::collections::HashSet<usize>,
    pinned_indices: std::collections::HashSet<usize>,
    view_cache: Mutex<HashMap<Filters, Arc<Vec<usize>>>>,
}

impl Archive {
    pub fn load(folder: &Path) -> Archive {
        let meta = read_meta(folder);
        let mut messages = load_jsonl_readonly(&folder.join("messages.jsonl"));
        // A hard kill during a merge can leave messages.jsonl unsorted; ids
        // are chronological snowflakes, so sort at load to keep it readable.
        let unsorted = messages.windows(2).any(|w| {
            match (
                w[0].get("id").and_then(Value::as_str),
                w[1].get("id").and_then(Value::as_str),
            ) {
                (Some(a), Some(b)) => snowflake_key(b) < snowflake_key(a),
                _ => false,
            }
        });
        if unsorted {
            messages.sort_by_key(|m| {
                m.get("id")
                    .and_then(Value::as_str)
                    .map(snowflake_key)
                    .unwrap_or(0)
            });
        }
        let index_by_id: HashMap<String, usize> = messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.get("id").and_then(Value::as_str).map(|id| (id.to_string(), i)))
            .collect();
        let mut attachments_by_id = HashMap::new();
        for entry in load_jsonl_readonly(&folder.join("manifest.jsonl")) {
            if let (Some(id), Some(path)) = (
                entry.get("attachment_id").and_then(Value::as_str),
                entry.get("local_path").and_then(Value::as_str),
            ) {
                if !path.is_empty() {
                    attachments_by_id.insert(id.to_string(), entry);
                }
            }
        }
        let mut attachments: Vec<Value> = attachments_by_id.values().cloned().collect();
        attachments.sort_by_key(|e| e.get("seq").and_then(Value::as_u64).unwrap_or(0));

        let attachment_indices = messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                if m
                    .get("attachment_ids")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        let link_indices = messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let content = m.get("content").and_then(Value::as_str).unwrap_or("");
                let lower = content.to_lowercase();
                if lower.contains("http://") || lower.contains("https://") {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        let pinned_indices = messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                if m.get("pinned").and_then(Value::as_bool).unwrap_or(false) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();

        let attachment_records = attachments
            .iter()
            .map(|entry| {
                let index = entry
                    .get("message_id")
                    .and_then(Value::as_str)
                    .and_then(|id| index_by_id.get(id).copied());
                let timestamp = index
                    .and_then(|i| messages[i].get("timestamp").and_then(Value::as_str))
                    .unwrap_or("");
                let date = if timestamp.len() >= 10 {
                    Some(timestamp[..10].to_string())
                } else {
                    None
                };
                let year = date
                    .as_deref()
                    .map(|d| d[..4].to_string())
                    .unwrap_or_else(|| "?".to_string());
                AttachmentRecord {
                    entry: entry.clone(),
                    year,
                    date,
                }
            })
            .collect();

        Archive {
            folder: folder.to_path_buf(),
            meta,
            messages,
            index_by_id,
            attachments_by_id,
            attachments,
            attachment_records,
            attachment_indices,
            link_indices,
            pinned_indices,
            view_cache: Mutex::new(HashMap::new()),
        }
    }

    // -- files --------------------------------------------------------------

    pub fn attachment_name(entry: &Value) -> String {
        entry
            .get("local_path")
            .and_then(Value::as_str)
            .and_then(|p| p.rsplit('/').next())
            .unwrap_or("")
            .to_string()
    }

    pub fn attachment_mime(name: &str) -> &'static str {
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "svg" => "image/svg+xml",
            "mp4" => "video/mp4",
            "webm" => "video/webm",
            "mov" => "video/quicktime",
            "mkv" => "video/x-matroska",
            "mp3" => "audio/mpeg",
            "ogg" => "audio/ogg",
            "wav" => "audio/wav",
            "flac" => "audio/flac",
            "m4a" => "audio/mp4",
            "opus" => "audio/opus",
            "txt" | "log" | "md" => "text/plain",
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "js" => "text/javascript",
            "json" => "application/json",
            "pdf" => "application/pdf",
            "zip" => "application/zip",
            _ => "application/octet-stream",
        }
    }

    /// Map a bare attachment filename to a real path inside the archive,
    /// refusing anything that escapes the attachments directory.
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name == "."
            || name == ".."
        {
            return None;
        }
        let base = self.folder.join("attachments").canonicalize().ok()?;
        let candidate = base.join(name).canonicalize().ok()?;
        if candidate.parent() != Some(base.as_path()) || !candidate.is_file() {
            return None;
        }
        Some(candidate)
    }

    // -- payloads -----------------------------------------------------------

    pub fn meta_payload(&self) -> Value {
        json!({
            "chat": self.meta.get("chat").cloned().unwrap_or_else(|| json!("archive")),
            "channel_id": self.meta.get("channel_id").cloned().unwrap_or(Value::Null),
            "total_messages": self.messages.len(),
            "total_attachments": self.attachments.len(),
            "first_timestamp": self.messages.first().and_then(|m| m.get("timestamp").cloned()).unwrap_or(Value::Null),
            "last_timestamp": self.messages.last().and_then(|m| m.get("timestamp").cloned()).unwrap_or(Value::Null),
        })
    }

    /// Message-facing view of one attachment, or None if it is not on disk.
    fn attachment_view(&self, attachment_id: &str) -> Option<Value> {
        let entry = self.attachments_by_id.get(attachment_id)?;
        let name = Self::attachment_name(entry);
        self.resolve(&name)?;
        Some(json!({
            "id": entry.get("attachment_id").cloned().unwrap_or(Value::Null),
            "filename": entry.get("filename").cloned().filter(|f| !f.is_null())
                .unwrap_or_else(|| json!(name)),
            "size": entry.get("size").cloned().unwrap_or(Value::Null),
            "url": format!("/media/{name}"),
        }))
    }

    // -- filtered views -----------------------------------------------------

    /// Indices (ascending) matching the filters.
    pub fn filtered_view(&self, filters: &Filters) -> Arc<Vec<usize>> {
        if let Ok(cache) = self.view_cache.lock() {
            if let Some(view) = cache.get(filters) {
                return view.clone();
            }
        }
        let mut view = Vec::new();
        for (i, message) in self.messages.iter().enumerate() {
            let timestamp = message.get("timestamp").and_then(Value::as_str).unwrap_or("");
            if let Some(from) = &filters.date_from {
                if timestamp < from.as_str() {
                    continue;
                }
            }
            if let Some(to) = &filters.date_to {
                if timestamp > to.as_str() {
                    continue;
                }
            }
            if filters.has_attachment && !self.attachment_indices.contains(&i) {
                continue;
            }
            if filters.has_link && !self.link_indices.contains(&i) {
                continue;
            }
            if filters.pinned && !self.pinned_indices.contains(&i) {
                continue;
            }
            view.push(i);
        }
        let view = Arc::new(view);
        if let Ok(mut cache) = self.view_cache.lock() {
            if cache.len() >= VIEW_CACHE_MAX {
                cache.clear();
            }
            cache.insert(filters.clone(), view.clone());
        }
        view
    }

    /// Filtered view or the full log, in the requested direction.
    pub fn ordered_view(&self, filters: Option<&Filters>, reverse: bool) -> Vec<usize> {
        let mut view: Vec<usize> = match filters {
            None => (0..self.messages.len()).collect(),
            Some(f) => self.filtered_view(f).as_ref().clone(),
        };
        if reverse {
            view.reverse();
        }
        view
    }

    /// Position of a message within the ordered view, or None if the message
    /// does not exist or is excluded by the filters.
    pub fn anchor_position(
        &self,
        message_id: &str,
        filters: Option<&Filters>,
        reverse: bool,
    ) -> Option<usize> {
        let index = *self.index_by_id.get(message_id)?;
        let ascending = self.ordered_view(filters, false);
        let position = ascending.binary_search(&index).ok()?;
        if ascending[position] != index {
            return None;
        }
        Some(if reverse {
            ascending.len() - 1 - position
        } else {
            position
        })
    }

    // -- API payloads ---------------------------------------------------------

    pub fn page_payload(
        &self,
        start: i64,
        count: usize,
        filters: Option<&Filters>,
        reverse: bool,
    ) -> Value {
        let ordered = self.ordered_view(filters, reverse);
        let total = ordered.len();
        let mut start = start;
        if start < 0 {
            // negative counts back from the end of the view
            start = (total as i64 + start).max(0);
        }
        let start = (start.max(0) as usize).min(total);
        let end = (start + count).min(total);
        let mut out = Vec::with_capacity(end - start);
        for &index in &ordered[start..end] {
            let message = &self.messages[index];
            let mut item = message.clone();
            // seq = position in the archive, not the platform message id
            item["seq"] = json!(index);
            let attachments: Vec<Value> = message
                .get("attachment_ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|a| a.as_str())
                        .filter_map(|a| self.attachment_view(a))
                        .collect()
                })
                .unwrap_or_default();
            item["attachments"] = json!(attachments);
            out.push(item);
        }
        json!({ "total": total, "start": start, "messages": out })
    }

    pub fn anchor_payload(
        &self,
        message_id: &str,
        count: usize,
        filters: Option<&Filters>,
        reverse: bool,
    ) -> Option<Value> {
        let position = self.anchor_position(message_id, filters, reverse)?;
        let start = position.saturating_sub(count / 2);
        let mut payload = self.page_payload(start as i64, count, filters, reverse);
        payload["anchor_index"] = json!(position);
        Some(payload)
    }

    pub fn resolve_payload(&self, ids: &[String]) -> Value {
        let mut out = serde_json::Map::new();
        for id in ids {
            if let Some(&index) = self.index_by_id.get(id) {
                let message = &self.messages[index];
                let content = message.get("content").and_then(Value::as_str).unwrap_or("");
                let snippet: String = content.chars().take(140).collect();
                out.insert(
                    message
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string(),
                    json!({
                        "index": index,
                        "author": message.get("author").cloned().unwrap_or(Value::Null),
                        "content": snippet,
                        "timestamp": message.get("timestamp").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
        }
        Value::Object(out)
    }

    pub fn search_payload(
        &self,
        query: &str,
        author: Option<&str>,
        filters: Option<&Filters>,
        limit: usize,
    ) -> Value {
        let query = query.to_lowercase();
        let author = author.map(|a| a.to_lowercase());
        let ordered = self.ordered_view(filters, false);
        let mut hits = Vec::new();
        // Most recent matches first: the view is ascending, scan it backwards.
        for &index in ordered.iter().rev() {
            let message = &self.messages[index];
            if let Some(author) = &author {
                if message
                    .get("author")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase()
                    != *author
                {
                    continue;
                }
            }
            let content = message.get("content").and_then(Value::as_str).unwrap_or("");
            if !content.to_lowercase().contains(&query) {
                continue;
            }
            hits.push(json!({
                "index": index,
                "id": message.get("id").cloned().unwrap_or(Value::Null),
                "author": message.get("author").cloned().unwrap_or(Value::Null),
                "timestamp": message.get("timestamp").cloned().unwrap_or(Value::Null),
                "content": content,
            }));
            if hits.len() >= limit {
                break;
            }
        }
        let truncated = hits.len() >= limit;
        json!({ "query": query, "results": hits, "truncated": truncated })
    }

    pub fn attachment_year_counts(&self) -> Value {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for rec in &self.attachment_records {
            *counts.entry(rec.year.as_str()).or_insert(0) += 1;
        }
        let mut years: Vec<(&str, usize)> = counts.into_iter().collect();
        years.sort_by(|a, b| b.0.cmp(a.0));
        json!(years
            .into_iter()
            .map(|(year, count)| json!({"year": year, "count": count}))
            .collect::<Vec<_>>())
    }

    pub fn attachments_payload(&self, start: usize, count: usize, year: Option<&str>) -> Value {
        let records: Vec<&AttachmentRecord> = match year {
            Some(y) => self.attachment_records.iter().filter(|r| r.year == y).collect(),
            None => self.attachment_records.iter().collect(),
        };
        let total = records.len();
        let start = start.min(total);
        let end = (start + count).min(total);
        let items: Vec<Value> = records[start..end]
            .iter()
            .map(|rec| {
                let entry = &rec.entry;
                let name = Self::attachment_name(entry);
                let message_id = entry.get("message_id").and_then(Value::as_str);
                json!({
                    "seq": entry.get("seq").cloned().unwrap_or(Value::Null),
                    "attachment_id": entry.get("attachment_id").cloned().unwrap_or(Value::Null),
                    "message_id": message_id,
                    "message_index": message_id
                        .and_then(|id| self.index_by_id.get(id).copied())
                        .map(|i| json!(i))
                        .unwrap_or(Value::Null),
                    "filename": entry.get("filename").cloned().unwrap_or(Value::Null),
                    "size": entry.get("size").cloned().unwrap_or(Value::Null),
                    "date": rec.date.clone().map(|d| json!(d)).unwrap_or(Value::Null),
                    "url": format!("/media/{name}"),
                })
            })
            .collect();
        json!({ "total": total, "start": start, "attachments": items })
    }

    pub fn stats_payload(&self) -> Value {
        let mut authors: HashMap<String, usize> = HashMap::new();
        let mut months: HashMap<String, usize> = HashMap::new();
        let mut edited = 0usize;
        let mut replies = 0usize;
        let mut pinned = 0usize;
        let mut with_attachments = 0usize;
        for message in &self.messages {
            *authors
                .entry(
                    message
                        .get("author")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                )
                .or_insert(0) += 1;
            let ts = message.get("timestamp").and_then(Value::as_str).unwrap_or("");
            let month = if ts.len() >= 7 { ts[..7].to_string() } else { String::new() };
            *months.entry(month).or_insert(0) += 1;
            if message
                .get("edited_timestamp")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                edited += 1;
            }
            if message
                .get("reply_to")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                replies += 1;
            }
            if message.get("pinned").and_then(Value::as_bool).unwrap_or(false) {
                pinned += 1;
            }
            if message
                .get("attachment_ids")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
            {
                with_attachments += 1;
            }
        }
        let mut extensions: HashMap<String, usize> = HashMap::new();
        let mut total_bytes = 0u64;
        for entry in &self.attachments {
            let filename = entry.get("filename").and_then(Value::as_str).unwrap_or("");
            let ext = filename
                .rsplit('.')
                .next()
                .filter(|_| filename.contains('.'))
                .map(|e| e.to_lowercase())
                .unwrap_or_else(|| "?".to_string());
            *extensions.entry(ext).or_insert(0) += 1;
            total_bytes += entry.get("size").and_then(Value::as_u64).unwrap_or(0);
        }
        let mut author_list: Vec<(&String, &usize)> = authors.iter().collect();
        author_list.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let mut month_list: Vec<(&String, &usize)> = months.iter().collect();
        month_list.sort_by(|a, b| a.0.cmp(b.0));
        let mut ext_list: Vec<(&String, &usize)> = extensions.iter().collect();
        ext_list.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        json!({
            "total_messages": self.messages.len(),
            "total_attachments": self.attachments.len(),
            "attachment_bytes": total_bytes,
            "authors": author_list.iter().map(|(name, count)| json!({"name": name, "count": count})).collect::<Vec<_>>(),
            "months": month_list.iter().map(|(month, count)| json!({"month": month, "count": count})).collect::<Vec<_>>(),
            "edited": edited,
            "replies": replies,
            "pinned": pinned,
            "with_attachments": with_attachments,
            "attachment_types": ext_list.iter().map(|(ext, count)| json!({"ext": ext, "count": count})).collect::<Vec<_>>(),
        })
    }
}

fn snowflake_key(id: &str) -> u128 {
    id.parse::<u128>().unwrap_or(0)
}

/// Load JSONL records read-only (the viewer never truncates the archive),
/// dropping a partial tail line left behind by a hard kill.
fn load_jsonl_readonly(path: &Path) -> Vec<Value> {
    let mut records = Vec::new();
    let Ok(bytes) = std::fs::read(path) else {
        return records;
    };
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(v) => records.push(v),
            Err(_) => break,
        }
    }
    records
}

/// Archive folders directly under root (meta.json + messages.jsonl), most
/// recently modified first.
pub fn find_archives(root: &Path) -> Vec<Value> {
    let mut archives = Vec::new();
    if let Ok(children) = std::fs::read_dir(root) {
        for entry in children.flatten() {
            let path = entry.path();
            if !path.is_dir()
                || !path.join("meta.json").is_file()
                || !path.join("messages.jsonl").is_file()
            {
                continue;
            }
            let meta = read_meta(&path);
            if !meta.is_object() {
                continue;
            }
            let mtime = path
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            archives.push(json!({
                "name": name,
                "chat": meta.get("chat").cloned().unwrap_or_else(|| json!(name)),
                "channel_id": meta.get("channel_id").cloned().unwrap_or(Value::Null),
                "mtime": mtime,
            }));
        }
    }
    archives.sort_by(|a, b| {
        b.get("mtime")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .partial_cmp(&a.get("mtime").and_then(Value::as_f64).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    archives
}

// ---------------------------------------------------------------------------
// Library (archival folder + small LRU of loaded archives + selection)
// ---------------------------------------------------------------------------

struct CacheState {
    map: HashMap<String, Arc<Archive>>,
    order: VecDeque<String>,
}

pub struct Library {
    root: PathBuf,
    cache: Mutex<CacheState>,
    selected: Mutex<Option<String>>,
}

impl Library {
    pub fn new(root: PathBuf) -> Library {
        Library {
            root,
            cache: Mutex::new(CacheState {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            selected: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The Archive for a bare folder name, or None if it is not an archive
    /// folder. Loads and caches on first use; does not change the selection.
    pub fn load(&self, name: &str) -> Option<Arc<Archive>> {
        // Only bare child names are accepted: this rejects traversal ("..",
        // embedded separators) without resolving, so symlinked archive
        // folders work.
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return None;
        }
        let folder = self.root.join(name);
        if folder.parent() != Some(self.root.as_path()) {
            return None;
        }
        if !folder.join("meta.json").is_file() || !folder.join("messages.jsonl").is_file() {
            return None;
        }
        let mut state = self.cache.lock().ok()?;
        if state.map.contains_key(name) {
            // refresh LRU position
            if let Some(pos) = state.order.iter().position(|n| n == name) {
                state.order.remove(pos);
            }
            state.order.push_back(name.to_string());
            return state.map.get(name).cloned();
        }
        let archive = Arc::new(Archive::load(&folder));
        state.order.push_back(name.to_string());
        state.map.insert(name.to_string(), archive.clone());
        while state.map.len() > LIBRARY_CACHE_MAX {
            match state.order.pop_front() {
                Some(oldest) => {
                    state.map.remove(&oldest);
                }
                None => break,
            }
        }
        Some(archive)
    }

    pub fn select(&self, name: &str) -> Option<Arc<Archive>> {
        let archive = self.load(name)?;
        if let Ok(mut selected) = self.selected.lock() {
            *selected = Some(name.to_string());
        }
        Some(archive)
    }

    pub fn selected_name(&self) -> Option<String> {
        self.selected.lock().ok().and_then(|g| g.clone())
    }

    pub fn selected(&self) -> Option<Arc<Archive>> {
        let name = self.selected_name()?;
        self.load(&name)
    }
}
