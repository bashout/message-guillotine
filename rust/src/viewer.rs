//! V2 local archive viewer: serves an archival folder over localhost HTTP
//! with an embedded single-page UI (archive selector, paged messages, search,
//! date/attachment/link/pinned filters, reply threading, media gallery, stats).
//!
//! The on-disk format is the one documented in `archive_format`; this module
//! only reads it. The JSON API mirrors the V1 Python viewer so both frontends
//! could be swapped against either server.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{middleware, Json, Router};
use serde_json::{json, Value};

use crate::archive_format::read_meta;

pub const PAGE_DEFAULT: usize = 50;
pub const PAGE_MAX: usize = 200;
pub const GALLERY_DEFAULT: usize = 60;
pub const SEARCH_MAX: usize = 200;
const VIEW_CACHE_MAX: usize = 32;
const LIBRARY_CACHE_MAX: usize = 2;

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Active message filters. `None` (via `Filters::is_empty`) means the
/// unfiltered full log. Timestamps are uniform UTC ISO strings, so lexical
/// comparison equals chronological comparison.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Filters {
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub has_attachment: bool,
    pub has_link: bool,
    pub pinned: bool,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.date_from.is_none()
            && self.date_to.is_none()
            && !self.has_attachment
            && !self.has_link
            && !self.pinned
    }
}

/// Turn query params into a filter set, or None when nothing is filtered.
/// Dates accept either YYYY-MM-DD or a full ISO timestamp; a bare end date
/// includes that whole day. Full ISO timestamps are normalized to UTC first
/// (a "+02:00" filter would otherwise sort wrongly against the archive's
/// "+00:00" strings); naive timestamps are taken as UTC.
pub fn parse_filters(q: &HashMap<String, String>) -> Option<Filters> {
    let date_from = q.get("from").map(|v| utc_iso(v));
    let mut date_to = q.get("to").map(|v| utc_iso(v));
    if let Some(to) = &date_to {
        if to.len() == 10 {
            date_to = Some(format!("{to}T23:59:59.999999+00:00"));
        }
    }
    let has_attachment = q.get("has_attachment").is_some_and(|v| v == "true");
    let has_link = q.get("has_link").is_some_and(|v| v == "true");
    let pinned = q.get("pinned").is_some_and(|v| v == "true");
    let filters = Filters {
        date_from,
        date_to,
        has_attachment,
        has_link,
        pinned,
    };
    if filters.is_empty() {
        None
    } else {
        Some(filters)
    }
}

/// A bare YYYY-MM-DD passes through; a full ISO timestamp comes back as a UTC
/// ISO string (Python-isoformat shaped). Unparseable values pass through
/// unchanged (they simply match nothing).
pub fn utc_iso(value: &str) -> String {
    if value.is_empty() || value.len() == 10 {
        return value.to_string();
    }
    let parsed: Option<chrono::DateTime<chrono::Utc>> = chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|ndt| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ndt, chrono::Utc))
        })
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|ndt| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ndt, chrono::Utc))
        });
    match parsed {
        Some(dt) => {
            use chrono::Timelike;
            if dt.nanosecond() == 0 {
                dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
            } else {
                dt.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
            }
        }
        None => value.to_string(),
    }
}

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
    messages: Vec<Value>,
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

    fn attachment_name(entry: &Value) -> String {
        entry
            .get("local_path")
            .and_then(Value::as_str)
            .and_then(|p| p.rsplit('/').next())
            .unwrap_or("")
            .to_string()
    }

    fn attachment_mime(name: &str) -> &'static str {
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

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

type ApiError = (StatusCode, Json<Value>);
type ApiResult = Result<Json<Value>, ApiError>;

fn json_err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(json!({ "error": msg })))
}

/// Minimal query-string parser (first value wins, like the V1 handler).
fn query_map(raw: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(raw) = raw else { return out };
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = percent_decode(k);
        out.entry(k).or_insert_with(|| percent_decode(v));
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn q_int(q: &HashMap<String, String>, name: &str, default: i64) -> Result<i64, ApiError> {
    match q.get(name) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<i64>()
            .map_err(|_| json_err(StatusCode::BAD_REQUEST, &format!("invalid {name}: {raw:?}"))),
    }
}

/// The archive for a request: the ?archive= pin when given, else the shared
/// selection.
fn request_archive(library: &Library, q: &HashMap<String, String>) -> Result<Arc<Archive>, ApiError> {
    if let Some(name) = q.get("archive") {
        if name.is_empty() {
            return Err(json_err(StatusCode::BAD_REQUEST, "archive parameter required"));
        }
        library
            .load(name)
            .ok_or_else(|| json_err(StatusCode::NOT_FOUND, "unknown archive"))
    } else {
        library
            .selected()
            .ok_or_else(|| json_err(StatusCode::CONFLICT, "no archive selected"))
    }
}

/// Reject non-local Host headers (DNS-rebinding protection). The Host
/// hostname must be an IP literal or "localhost".
fn host_allowed(host: &str) -> bool {
    let host = host.trim().to_lowercase();
    if host.is_empty() {
        return false;
    }
    let name = if host.starts_with('[') {
        match host.find(']') {
            Some(end) => &host[1..end],
            None => return false,
        }
    } else if host
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        host.rsplit_once(':').unwrap().0
    } else {
        host.as_str()
    };
    if name == "localhost" {
        return true;
    }
    name.parse::<std::net::IpAddr>().is_ok()
}

/// Baseline security headers on every viewer response.
async fn security_headers(req: axum::extract::Request, next: middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; img-src 'self' data:; media-src 'self'; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    h.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    resp
}

async fn host_guard(req: axum::extract::Request, next: middleware::Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if host_allowed(host) {
        next.run(req).await
    } else {
        json_err(StatusCode::FORBIDDEN, "forbidden host").into_response()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index_html() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML).into_response()
}

async fn app_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        APP_JS,
    )
        .into_response()
}

async fn style_css() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE_CSS).into_response()
}

async fn api_archives(State(library): State<Arc<Library>>) -> Json<Value> {
    let mut archives = find_archives(library.root());
    for entry in archives.iter_mut() {
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("mtime");
        }
    }
    Json(json!({
        "archives": archives,
        "selected": library.selected_name(),
        "root": library.root().to_string_lossy(),
    }))
}

async fn api_select(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let name = q
        .get("archive")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| json_err(StatusCode::BAD_REQUEST, "archive parameter required"))?;
    let archive = library
        .select(name)
        .ok_or_else(|| json_err(StatusCode::NOT_FOUND, "unknown archive"))?;
    let mut payload = archive.meta_payload();
    payload["archive"] = json!(name);
    Ok(Json(payload))
}

async fn api_meta(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    Ok(Json(archive.meta_payload()))
}

async fn api_messages(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    let count = q_int(&q, "count", PAGE_DEFAULT as i64)?.clamp(1, PAGE_MAX as i64) as usize;
    let filters = parse_filters(&q);
    let reverse = q.get("dir").is_some_and(|d| d == "desc");
    if let Some(anchor) = q.get("anchor") {
        let payload = archive
            .anchor_payload(anchor, count, filters.as_ref(), reverse)
            .ok_or_else(|| json_err(StatusCode::NOT_FOUND, "unknown message id"))?;
        return Ok(Json(payload));
    }
    let start = q_int(&q, "start", 0)?;
    Ok(Json(archive.page_payload(start, count, filters.as_ref(), reverse)))
}

async fn api_resolve(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    let ids: Vec<String> = q
        .get("ids")
        .map(|s| s.split(',').take(100).map(|s| s.to_string()).collect())
        .unwrap_or_default();
    Ok(Json(archive.resolve_payload(&ids)))
}

async fn api_search(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    let query = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    if query.is_empty() {
        return Ok(Json(json!({ "query": "", "results": [], "truncated": false })));
    }
    let author = q.get("author").map(|s| s.as_str());
    let filters = parse_filters(&q);
    Ok(Json(archive.search_payload(&query, author, filters.as_ref(), SEARCH_MAX)))
}

async fn api_attachment_years(
    State(library): State<Arc<Library>>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    Ok(Json(json!({ "years": archive.attachment_year_counts() })))
}

async fn api_attachments(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    let count = q_int(&q, "count", GALLERY_DEFAULT as i64)?.clamp(1, PAGE_MAX as i64) as usize;
    let start = q_int(&q, "start", 0)?.max(0) as usize;
    let year = q.get("year").map(|s| s.as_str());
    Ok(Json(archive.attachments_payload(start, count, year)))
}

async fn api_stats(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    Ok(Json(archive.stats_payload()))
}

enum ByteRange {
    Full,
    Partial(u64, u64), // start, end inclusive
    Unsatisfiable,
}

fn parse_range(header_value: &str, size: u64) -> ByteRange {
    if size == 0 {
        return ByteRange::Unsatisfiable;
    }
    let h = header_value.trim();
    let Some(rest) = h.strip_prefix("bytes=") else {
        return ByteRange::Full;
    };
    let (a, b) = rest.split_once('-').unwrap_or((rest, ""));
    if a.is_empty() && b.is_empty() {
        return ByteRange::Full;
    }
    if a.is_empty() {
        // suffix range: last N bytes
        let Ok(n) = b.parse::<u64>() else {
            return ByteRange::Full;
        };
        if n == 0 {
            return ByteRange::Unsatisfiable;
        }
        let start = size.saturating_sub(n);
        return if start >= size {
            ByteRange::Unsatisfiable
        } else {
            ByteRange::Partial(start, size - 1)
        };
    }
    let Ok(start) = a.parse::<u64>() else {
        return ByteRange::Full;
    };
    let mut end = size - 1;
    if !b.is_empty() {
        if let Ok(e) = b.parse::<u64>() {
            end = e.min(size - 1);
        }
    }
    if start > end || start >= size {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Partial(start, end)
}

/// Serve an attachment with single-range support so <video>/<audio> can
/// seek. Only plain media (image/video/audio, except SVG) is rendered inline;
/// everything else is served as a download, because an archived file that
/// renders in the viewer's origin could run its script against every archive
/// the server exposes.
async fn media(
    State(library): State<Arc<Library>>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let q = query_map(raw.as_deref());
    let archive = match request_archive(&library, &q) {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    let fs_path = match archive.resolve(&name) {
        Some(p) => p,
        None => return json_err(StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let mime = Archive::attachment_mime(&name);
    let download = q.contains_key("download");
    let inline = (mime.starts_with("image/")
        || mime.starts_with("video/")
        || mime.starts_with("audio/"))
        && mime != "image/svg+xml";
    let disposition = if download || !inline {
        format!("attachment; filename=\"{}\"", name.replace('"', ""))
    } else {
        "inline".to_string()
    };
    let size = match tokio::fs::metadata(&fs_path).await {
        Ok(m) => m.len(),
        Err(_) => return json_err(StatusCode::NOT_FOUND, "not found").into_response(),
    };

    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let (status, start, length, content_range) = match range_header
        .as_deref()
        .map(|h| parse_range(h, size))
    {
        Some(ByteRange::Unsatisfiable) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap();
        }
        Some(ByteRange::Partial(s, e)) => (
            StatusCode::PARTIAL_CONTENT,
            s,
            e - s + 1,
            Some(format!("bytes {s}-{e}/{size}")),
        ),
        _ => (StatusCode::OK, 0, size, None),
    };

    let file = match tokio::fs::File::open(&fs_path).await {
        Ok(f) => f,
        Err(_) => return json_err(StatusCode::NOT_FOUND, "not found").into_response(),
    };
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = file;
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return json_err(StatusCode::NOT_FOUND, "not found").into_response();
    }
    let stream = futures::stream::try_unfold((file, length), |(mut f, mut remaining)| async move {
        if remaining == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        let mut buf = vec![0u8; remaining.min(256 * 1024) as usize];
        let n = f.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        remaining -= n as u64;
        Ok(Some((axum::body::Bytes::from(buf), (f, remaining))))
    });

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header("x-content-type-options", "nosniff")
        .header("content-security-policy", "sandbox");
    if let Some(cr) = content_range {
        builder = builder.header(header::CONTENT_RANGE, cr);
    }
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::empty())
            .unwrap()
    })
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

fn default_root() -> PathBuf {
    if let Some(p) = std::env::var_os(crate::config::ENV_ARCHIVE_DIR) {
        return PathBuf::from(p);
    }
    let cfg_path = std::env::var_os(crate::config::ENV_CONFIG)
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::default_config_path);
    if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
        if let Ok(toml) = raw.parse::<toml::Value>() {
            if let Some(dir) = toml.get("archive_dir").and_then(|v| v.as_str()) {
                return PathBuf::from(dir);
            }
        }
    }
    crate::config::default_archive_dir()
}

fn open_browser(url: &str) {
    let result = {
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("xdg-open").arg(url).spawn()
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(url).spawn()
        }
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("cmd").args(["/C", "start", url]).spawn()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no browser opener",
            ))
        }
    };
    if let Err(e) = result {
        tracing::warn!("could not open the browser: {e}; open {url} manually");
    }
}

/// Run the V2 archive viewer server. `archives_dir` may be the archival
/// folder or a single archive folder (served with its siblings); when None
/// the folder is resolved like the archiver does (DM_ARCHIVE_DIR, config,
/// per-OS default).
pub async fn run(
    archives_dir: Option<PathBuf>,
    host: &str,
    port: u16,
    no_browser: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let (root, initial): (PathBuf, Option<String>) = match archives_dir {
        Some(target) => {
            if target.join("meta.json").is_file() && target.join("messages.jsonl").is_file() {
                // a single archive was passed: serve it with its siblings
                let name = target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .context("archive folder has no name")?;
                let parent = target
                    .parent()
                    .map(|p| p.to_path_buf())
                    .context("archive folder has no parent")?;
                (parent, Some(name))
            } else {
                (target, None)
            }
        }
        None => {
            let root = default_root();
            if find_archives(&root).is_empty() {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                if !find_archives(&cwd).is_empty() {
                    eprintln!(
                        "note: no archives in {}; falling back to the current directory",
                        root.display()
                    );
                    return Box::pin(run(Some(cwd), host, port, no_browser)).await;
                }
            }
            (root, None)
        }
    };
    let root = root.canonicalize().unwrap_or(root);
    let library = Arc::new(Library::new(root));

    let archives = find_archives(library.root());
    if let Some(name) = &initial {
        if archives.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(name)) {
            library.select(name);
        }
    }
    if library.selected_name().is_none() {
        if let Some(first) = archives.first().and_then(|a| a.get("name").and_then(Value::as_str)) {
            library.select(first);
        }
    }

    let app = Router::new()
        .route("/", axum::routing::get(index_html))
        .route("/app.js", axum::routing::get(app_js))
        .route("/style.css", axum::routing::get(style_css))
        .route("/api/archives", axum::routing::get(api_archives))
        .route("/api/select", axum::routing::get(api_select))
        .route("/api/meta", axum::routing::get(api_meta))
        .route("/api/messages", axum::routing::get(api_messages))
        .route("/api/resolve", axum::routing::get(api_resolve))
        .route("/api/search", axum::routing::get(api_search))
        .route("/api/attachment-years", axum::routing::get(api_attachment_years))
        .route("/api/attachments", axum::routing::get(api_attachments))
        .route("/api/stats", axum::routing::get(api_stats))
        .route("/media/{*name}", axum::routing::get(media))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(host_guard))
        .with_state(library.clone());

    if !host.is_empty() && host != "127.0.0.1" && host != "localhost" && host != "::1" && host != "[::1]" {
        eprintln!(
            "WARNING: viewer is bound to {host} — all archives are readable by anyone who can reach this port with no authentication. Use the default 127.0.0.1 unless you know what you're doing."
        );
    }
    let listener = match tokio::net::TcpListener::bind((host, port)).await {
        Ok(l) => l,
        Err(e) if port != 0 => {
            tracing::warn!("could not bind {host}:{port} ({e}); falling back to an ephemeral port");
            tokio::net::TcpListener::bind((host, 0))
                .await
                .context("could not bind the viewer to any port")?
        }
        Err(e) => return Err(e).context("could not bind the viewer"),
    };
    let local: SocketAddr = listener.local_addr()?;
    let url = format!("http://{}:{}/", local.ip(), local.port());

    eprintln!("Archival folder: {}", library.root().display());
    eprintln!("  {} archive(s) found", archives.len());
    if let Some(archive) = library.selected() {
        let meta = archive.meta_payload();
        eprintln!(
            "  {} messages, {} attachments",
            meta.get("total_messages").and_then(Value::as_u64).unwrap_or(0),
            meta.get("total_attachments").and_then(Value::as_u64).unwrap_or(0),
        );
    } else {
        eprintln!("  nothing to serve yet; archive a chat first");
    }
    eprintln!("Serving at {url} (Ctrl-C to stop)");
    if !no_browser {
        open_browser(&url);
    }
    axum::serve(listener, app)
        .await
        .context("viewer server error")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Embedded single-page UI (served from memory, no external files)
// ---------------------------------------------------------------------------
const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>guillotine archive viewer</title>
<link rel="stylesheet" href="/style.css">
</head>
<body>
<header id="topbar">
  <select id="archive-select" title="Archive"></select>
  <input id="search" type="search" placeholder="Search messages&hellip;">
  <button id="search-btn">Search</button>
  <button id="search-clear" class="hidden">Clear</button>
  <span class="spacer"></span>
  <button id="view-messages" class="tab active">Messages</button>
  <button id="view-gallery" class="tab">Media</button>
  <button id="view-stats" class="tab">Stats</button>
</header>
<div id="filter-bar">
  <label>From <input type="date" id="date-from"></label>
  <label>To <input type="date" id="date-to"></label>
  <label><input type="checkbox" id="f-att"> attachments</label>
  <label><input type="checkbox" id="f-link"> links</label>
  <label><input type="checkbox" id="f-pin"> pinned</label>
  <span id="view-info"></span>
</div>
<main>
  <section id="pane-messages">
    <div class="pager" id="pager-top"></div>
    <div id="message-list"></div>
    <div class="pager" id="pager-bottom"></div>
  </section>
  <section id="pane-gallery" class="hidden">
    <div id="year-bar"></div>
    <div id="gallery"></div>
    <div class="pager"><button id="gallery-more">Load more</button></div>
  </section>
  <section id="pane-stats" class="hidden"><div id="stats"></div></section>
</main>
<script src="/app.js"></script>
</body>
</html>
"##;

const STYLE_CSS: &str = r##"* { box-sizing: border-box; }
:root {
  --bg: #14161a; --panel: #1c1f26; --fg: #d7dae0; --fg-dim: #8a8f9a;
  --accent: #6ea8fe; --line: #2a2e37;
}
body { margin: 0; background: var(--bg); color: var(--fg);
  font: 14px/1.45 -apple-system, "Segoe UI", Roboto, sans-serif; }
#topbar { display: flex; gap: 8px; align-items: center; padding: 8px 12px;
  background: var(--panel); border-bottom: 1px solid var(--line); flex-wrap: wrap; }
#topbar select, #topbar input, #filter-bar input { background: var(--bg); color: var(--fg);
  border: 1px solid var(--line); border-radius: 6px; padding: 5px 8px; }
#search { flex: 1; min-width: 160px; }
button { background: var(--panel); color: var(--fg); border: 1px solid var(--line);
  border-radius: 6px; padding: 5px 12px; cursor: pointer; }
button:hover { border-color: var(--accent); }
.tab.active { border-color: var(--accent); color: var(--accent); }
.spacer { flex: 1; }
#filter-bar { display: flex; gap: 14px; align-items: center; padding: 6px 12px;
  border-bottom: 1px solid var(--line); color: var(--fg-dim); flex-wrap: wrap; }
#filter-bar label { display: flex; gap: 5px; align-items: center; }
#view-info { margin-left: auto; color: var(--fg-dim); }
main { padding: 12px; max-width: 960px; margin: 0 auto; }
.hidden { display: none !important; }
.pager { display: flex; gap: 8px; justify-content: center; padding: 10px 0; color: var(--fg-dim); }
.msg { background: var(--panel); border: 1px solid var(--line); border-radius: 8px;
  padding: 8px 12px; margin-bottom: 8px; }
.msg.anchor { border-color: var(--accent); }
.msg .head { display: flex; gap: 8px; align-items: baseline; color: var(--fg-dim); font-size: 12px; }
.msg .author { color: var(--fg); font-weight: 600; font-size: 14px; }
.msg .pinned { color: var(--accent); font-size: 11px; }
.msg .content { white-space: pre-wrap; word-break: break-word; margin-top: 2px; }
.msg .content a { color: var(--accent); }
.reply-line { margin-top: 4px; font-size: 12px; color: var(--fg-dim); cursor: pointer;
  border-left: 2px solid var(--line); padding-left: 8px; }
.reply-line:hover { color: var(--fg); }
.reply-author { font-weight: 600; }
.atts { margin-top: 6px; display: flex; flex-wrap: wrap; gap: 8px; }
.atts img { max-width: 220px; max-height: 160px; border-radius: 6px; border: 1px solid var(--line); }
.atts a { color: var(--accent); }
#year-bar { display: flex; gap: 6px; flex-wrap: wrap; margin-bottom: 10px; }
.chip { border: 1px solid var(--line); border-radius: 999px; padding: 3px 10px;
  cursor: pointer; color: var(--fg-dim); background: var(--panel); }
.chip.active { color: var(--accent); border-color: var(--accent); }
#gallery { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 10px; }
.g-item { background: var(--panel); border: 1px solid var(--line); border-radius: 8px;
  padding: 6px; cursor: pointer; overflow: hidden; }
.g-item img, .g-item video { width: 100%; border-radius: 4px; display: block; }
.g-item .g-name { font-size: 11px; color: var(--fg-dim); word-break: break-all; margin-top: 4px; }
#stats h3 { margin-bottom: 4px; }
#stats .stat-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 8px; margin-bottom: 16px; }
#stats .stat { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 8px 10px; }
#stats .stat .n { font-size: 20px; font-weight: 700; }
#stats .stat .l { color: var(--fg-dim); font-size: 11px; }
#stats table { border-collapse: collapse; margin-bottom: 16px; }
#stats td, #stats th { border-bottom: 1px solid var(--line); padding: 3px 12px 3px 0; text-align: left; font-size: 13px; }
#stats th { color: var(--fg-dim); font-weight: 500; }
"##;
const APP_JS: &str = r##"'use strict';
const PAGE = 50, GALLERY_PAGE = 60;
const state = {
  archive: null,
  start: 0, total: 0,
  mode: 'messages',
  searching: false,
  galleryLoaded: 0, galleryTotal: 0, year: null,
  replyCache: new Map(),
};
const $ = (id) => document.getElementById(id);
const els = {
  select: $('archive-select'), search: $('search'), searchBtn: $('search-btn'), searchClear: $('search-clear'),
  dateFrom: $('date-from'), dateTo: $('date-to'), fAtt: $('f-att'), fLink: $('f-link'), fPin: $('f-pin'),
  viewInfo: $('view-info'), list: $('message-list'), pagerTop: $('pager-top'), pagerBottom: $('pager-bottom'),
  gallery: $('gallery'), yearBar: $('year-bar'), galleryMore: $('gallery-more'), stats: $('stats'),
};

async function api(path) {
  let url = path;
  if (state.archive) url += (path.includes('?') ? '&' : '?') + 'archive=' + encodeURIComponent(state.archive);
  const r = await fetch(url);
  if (!r.ok) {
    let msg = r.statusText;
    try { msg = (await r.json()).error || msg; } catch (e) {}
    throw new Error(msg);
  }
  return r.json();
}

function filterQS() {
  const p = [];
  if (els.dateFrom.value) p.push('from=' + els.dateFrom.value);
  if (els.dateTo.value) p.push('to=' + els.dateTo.value);
  if (els.fAtt.checked) p.push('has_attachment=true');
  if (els.fLink.checked) p.push('has_link=true');
  if (els.fPin.checked) p.push('pinned=true');
  return p.length ? '&' + p.join('&') : '';
}

function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}
function fmtTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  return isNaN(d) ? esc(ts) : d.toLocaleString();
}
function linkify(text) {
  return esc(text).replace(/https?:\/\/\S+/gi, (m) => '<a href="' + m + '" target="_blank" rel="noopener">' + m + '</a>');
}
function extOf(name) {
  const m = /\.([A-Za-z0-9]{1,5})$/.exec(name || '');
  return m ? m[1].toLowerCase() : '';
}
const IMAGE_EXTS = new Set(['png','jpg','jpeg','gif','webp','bmp']);
const VIDEO_EXTS = new Set(['mp4','webm','mov','mkv']);

// -- archives ---------------------------------------------------------------
async function loadArchives() {
  const data = await api('/api/archives');
  els.select.innerHTML = '';
  for (const a of data.archives) {
    const opt = document.createElement('option');
    opt.value = a.name;
    opt.textContent = a.chat + ' (' + a.name + ')';
    els.select.appendChild(opt);
  }
  if (data.selected) {
    state.archive = data.selected;
    els.select.value = data.selected;
  } else if (data.archives.length) {
    await selectArchive(data.archives[0].name);
    return;
  }
  await refresh();
}

async function selectArchive(name) {
  await api('/api/select?archive=' + encodeURIComponent(name));
  state.archive = name;
  state.start = 0; state.searching = false;
  els.select.value = name;
  await refresh();
}

// -- messages ---------------------------------------------------------------
async function loadMessages(start) {
  const data = await api(`/api/messages?start=${start}&count=${PAGE}${filterQS()}`);
  state.start = data.start; state.total = data.total;
  renderMessages(data.messages, null);
  renderPagers();
  els.viewInfo.textContent = `${data.total} message(s)` + (filterQS() ? ' (filtered)' : '');
}

async function jumpTo(id) {
  const data = await api(`/api/messages?anchor=${encodeURIComponent(id)}&count=${PAGE}${filterQS()}`);
  state.start = data.start; state.total = data.total;
  renderMessages(data.messages, id);
  renderPagers();
  const el = els.list.querySelector('.msg.anchor');
  if (el) el.scrollIntoView({ block: 'center' });
}

function renderMessages(messages, anchorId) {
  els.list.innerHTML = '';
  for (const msg of messages) els.list.appendChild(renderMessage(msg, anchorId === msg.id));
  if (!messages.length) els.list.innerHTML = '<p style="color:var(--fg-dim)">No messages.</p>';
}

function renderMessage(msg, isAnchor) {
  const wrap = document.createElement('div');
  wrap.className = 'msg' + (isAnchor ? ' anchor' : '');
  wrap.dataset.id = msg.id;
  const head = document.createElement('div');
  head.className = 'head';
  head.innerHTML = `<span class="author">${esc(msg.author)}</span><span>${fmtTime(msg.timestamp)}${msg.edited_timestamp ? ' (edited)' : ''}</span>` +
    (msg.pinned ? '<span class="pinned">pinned</span>' : '');
  wrap.appendChild(head);
  if (msg.reply_to) wrap.appendChild(replyLine(msg.reply_to));
  const content = document.createElement('div');
  content.className = 'content';
  content.innerHTML = linkify(msg.content || '');
  wrap.appendChild(content);
  if (msg.attachments && msg.attachments.length) {
    const atts = document.createElement('div');
    atts.className = 'atts';
    for (const att of msg.attachments) {
      if (IMAGE_EXTS.has(extOf(att.filename || att.url))) {
        const img = document.createElement('img');
        img.src = att.url; img.alt = att.filename || ''; img.loading = 'lazy';
        atts.appendChild(img);
      } else {
        const a = document.createElement('a');
        a.href = att.url + '?download'; a.textContent = att.filename || att.url;
        atts.appendChild(a);
      }
    }
    wrap.appendChild(atts);
  }
  return wrap;
}

function replyLine(replyId) {
  const line = document.createElement('div');
  line.className = 'reply-line';
  line.innerHTML = '<span>&#8627; </span><span class="reply-body">loading&hellip;</span>';
  line.addEventListener('click', () => jumpTo(replyId));
  resolveReply(replyId).then((info) => {
    const body = line.querySelector('.reply-body');
    if (info) {
      body.innerHTML = `<span class="reply-author">${esc(info.author || 'unknown')}</span>: ${esc((info.content || '').slice(0, 80))}`;
    } else {
      body.textContent = '(message not in archive)';
    }
  });
  return line;
}

async function resolveReply(id) {
  if (state.replyCache.has(id)) return state.replyCache.get(id);
  try {
    const data = await api('/api/resolve?ids=' + encodeURIComponent(id));
    const info = data[id] || null;
    state.replyCache.set(id, info);
    return info;
  } catch (e) { return null; }
}

function renderPagers() {
  const mk = () => {
    const frag = document.createDocumentFragment();
    const newer = document.createElement('button');
    newer.textContent = 'Newer';
    newer.disabled = state.start <= 0;
    newer.addEventListener('click', () => loadMessages(Math.max(0, state.start - PAGE)));
    const older = document.createElement('button');
    older.textContent = 'Older';
    older.disabled = state.start + PAGE >= state.total;
    older.addEventListener('click', () => loadMessages(state.start + PAGE));
    frag.appendChild(newer); frag.appendChild(older);
    return frag;
  };
  els.pagerTop.innerHTML = ''; els.pagerTop.appendChild(mk());
  els.pagerBottom.innerHTML = ''; els.pagerBottom.appendChild(mk());
}

// -- search -----------------------------------------------------------------
async function doSearch() {
  const q = els.search.value.trim();
  if (!q) return;
  state.searching = true;
  els.searchClear.classList.remove('hidden');
  const data = await api('/api/search?q=' + encodeURIComponent(q) + filterQS());
  els.list.innerHTML = '';
  if (!data.results.length) {
    els.list.innerHTML = '<p style="color:var(--fg-dim)">No matches.</p>';
  }
  for (const r of data.results) {
    const wrap = document.createElement('div');
    wrap.className = 'msg';
    wrap.innerHTML = `<div class="head"><span class="author">${esc(r.author)}</span><span>${fmtTime(r.timestamp)}</span></div>` +
      `<div class="content">${linkify(r.content)}</div>`;
    wrap.addEventListener('click', () => jumpTo(r.id));
    els.list.appendChild(wrap);
  }
  els.pagerTop.innerHTML = ''; els.pagerBottom.innerHTML = '';
  els.viewInfo.textContent = `${data.results.length} match(es)${data.truncated ? ' (truncated)' : ''} for "${esc(data.query)}"`;
}

function clearSearch() {
  state.searching = false;
  els.search.value = '';
  els.searchClear.classList.add('hidden');
  loadMessages(0);
}

// -- gallery ----------------------------------------------------------------
async function loadYears() {
  const data = await api('/api/attachment-years');
  els.yearBar.innerHTML = '';
  const all = document.createElement('span');
  all.className = 'chip' + (state.year === null ? ' active' : '');
  all.textContent = 'all';
  all.addEventListener('click', () => { state.year = null; state.galleryLoaded = 0; refreshGallery(); });
  els.yearBar.appendChild(all);
  for (const y of data.years) {
    const chip = document.createElement('span');
    chip.className = 'chip' + (state.year === y.year ? ' active' : '');
    chip.textContent = `${y.year} (${y.count})`;
    chip.addEventListener('click', () => { state.year = y.year; state.galleryLoaded = 0; refreshGallery(); });
    els.yearBar.appendChild(chip);
  }
}

async function refreshGallery() {
  await loadYears();
  els.gallery.innerHTML = '';
  state.galleryLoaded = 0;
  await loadMoreGallery();
}

async function loadMoreGallery() {
  const yq = state.year ? `&year=${encodeURIComponent(state.year)}` : '';
  const data = await api(`/api/attachments?start=${state.galleryLoaded}&count=${GALLERY_PAGE}${yq}`);
  state.galleryLoaded = data.start + data.attachments.length;
  state.galleryTotal = data.total;
  els.galleryMore.disabled = state.galleryLoaded >= data.total;
  for (const att of data.attachments) {
    const item = document.createElement('div');
    item.className = 'g-item';
    const ext = extOf(att.filename || att.url);
    if (IMAGE_EXTS.has(ext)) {
      const img = document.createElement('img');
      img.src = att.url; img.loading = 'lazy';
      item.appendChild(img);
    } else if (VIDEO_EXTS.has(ext)) {
      const v = document.createElement('video');
      v.src = att.url; v.controls = true; v.preload = 'none';
      item.appendChild(v);
    }
    const name = document.createElement('div');
    name.className = 'g-name';
    name.textContent = `${att.filename || att.url} (${att.date || '?'})`;
    item.appendChild(name);
    if (att.message_id) item.addEventListener('click', () => { showPane('messages'); jumpTo(att.message_id); });
    els.gallery.appendChild(item);
  }
}

// -- stats ------------------------------------------------------------------
async function loadStats() {
  const s = await api('/api/stats');
  const grid = (items) => '<div class="stat-grid">' + items.map(([l, n]) =>
    `<div class="stat"><div class="n">${n}</div><div class="l">${esc(l)}</div></div>`).join('') + '</div>';
  const table = (title, rows, cols) => {
    if (!rows.length) return '';
    return `<h3>${esc(title)}</h3><table><tr>${cols.map((c) => `<th>${esc(c)}</th>`).join('')}</tr>` +
      rows.map((r) => '<tr>' + cols.map((c) => `<td>${esc(r[c])}</td>`).join('') + '</tr>').join('') + '</table>';
  };
  els.stats.innerHTML =
    grid([
      ['messages', s.total_messages],
      ['attachments', s.total_attachments],
      ['attachment bytes', s.attachment_bytes],
      ['edited', s.edited], ['replies', s.replies], ['pinned', s.pinned], ['with attachments', s.with_attachments],
    ]) +
    table('Authors', s.authors, ['name', 'count']) +
    table('Messages per month', s.months, ['month', 'count']) +
    table('Attachment types', s.attachment_types, ['ext', 'count']);
}

// -- wiring -----------------------------------------------------------------
function showPane(mode) {
  state.mode = mode;
  for (const [btn, pane] of [['view-messages', 'pane-messages'], ['view-gallery', 'pane-gallery'], ['view-stats', 'pane-stats']]) {
    $(btn).classList.toggle('active', btn === 'view-' + mode);
    $(pane).classList.toggle('hidden', btn !== 'view-' + mode);
  }
  if (mode === 'gallery') refreshGallery();
  if (mode === 'stats') loadStats();
}

async function refresh() {
  if (state.searching && els.search.value.trim()) return doSearch();
  if (state.mode === 'messages') await loadMessages(state.start || 0);
  else if (state.mode === 'gallery') await refreshGallery();
  else await loadStats();
}

els.select.addEventListener('change', () => selectArchive(els.select.value));
els.searchBtn.addEventListener('click', doSearch);
els.searchClear.addEventListener('click', clearSearch);
els.search.addEventListener('keydown', (e) => { if (e.key === 'Enter') doSearch(); });
for (const el of [els.dateFrom, els.dateTo, els.fAtt, els.fLink, els.fPin]) {
  el.addEventListener('change', () => { state.start = 0; refresh(); });
}
$('view-messages').addEventListener('click', () => showPane('messages'));
$('view-gallery').addEventListener('click', () => showPane('gallery'));
$('view-stats').addEventListener('click', () => showPane('stats'));
els.galleryMore.addEventListener('click', loadMoreGallery);

loadArchives().catch((e) => { els.viewInfo.textContent = 'Error: ' + e.message; });
"##;
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn mk_msg(id: &str, author: &str, ts: &str, content: &str) -> Value {
        json!({
            "id": id,
            "author_id": "u1",
            "author": author,
            "timestamp": ts,
            "edited_timestamp": Value::Null,
            "pinned": false,
            "content": content,
            "type": 0,
            "reply_to": Value::Null,
            "attachment_ids": [],
        })
    }

    struct Fixture {
        root: PathBuf,
        archive_dir: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "guillotine-viewer-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let archive_dir = root.join("chat-one");
        std::fs::create_dir_all(archive_dir.join("attachments")).unwrap();
        std::fs::write(
            archive_dir.join("meta.json"),
            r#"{"chat": "Chat One", "channel_id": "111"}"#,
        )
        .unwrap();
        let messages = vec![
            mk_msg("100", "alice", "2023-05-01T10:00:00.000000+00:00", "hello world"),
            mk_msg("101", "bob", "2023-05-02T10:00:00.000000+00:00", "see https://example.com now"),
            {
                let mut m = mk_msg("102", "alice", "2024-01-15T12:30:00.000000+00:00", "a reply to 100");
                m["reply_to"] = json!("100");
                m["pinned"] = json!(true);
                m["edited_timestamp"] = json!("2024-01-15T13:00:00.000000+00:00");
                m["attachment_ids"] = json!(["a1"]);
                m
            },
            mk_msg("103", "carol", "2024-06-20T08:00:00.000000+00:00", "HELLO again"),
        ];
        let mut jsonl = String::new();
        for m in &messages {
            jsonl.push_str(&serde_json::to_string(m).unwrap());
            jsonl.push('\n');
        }
        // partial tail line must be dropped
        jsonl.push_str("{\"id\": \"10");
        std::fs::write(archive_dir.join("messages.jsonl"), jsonl).unwrap();
        let manifest = json!({
            "seq": 1,
            "message_id": "102",
            "attachment_id": "a1",
            "filename": "photo.png",
            "local_path": "attachments/0001_102_photo.png",
            "size": 4,
            "status": "ok",
        });
        let mut f = std::fs::File::create(archive_dir.join("manifest.jsonl")).unwrap();
        writeln!(f, "{}", serde_json::to_string(&manifest).unwrap()).unwrap();
        std::fs::write(archive_dir.join("attachments/0001_102_photo.png"), b"fake").unwrap();
        Fixture { root, archive_dir }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn filters(
        from: Option<&str>,
        to: Option<&str>,
        att: bool,
        link: bool,
        pin: bool,
    ) -> Filters {
        Filters {
            date_from: from.map(|s| s.to_string()),
            date_to: to.map(|s| s.to_string()),
            has_attachment: att,
            has_link: link,
            pinned: pin,
        }
    }

    #[test]
    fn utc_iso_normalizes_offsets_and_passes_dates() {
        assert_eq!(utc_iso("2024-01-15"), "2024-01-15");
        assert_eq!(
            utc_iso("2024-01-15T14:30:00+02:00"),
            "2024-01-15T12:30:00+00:00"
        );
        assert_eq!(
            utc_iso("2024-01-15T12:30:00.500000Z"),
            "2024-01-15T12:30:00.500000+00:00"
        );
        assert_eq!(utc_iso("2024-01-15T12:30:00"), "2024-01-15T12:30:00+00:00");
        assert_eq!(utc_iso("not a date at all"), "not a date at all");
    }

    #[test]
    fn parse_filters_bare_end_date_covers_whole_day() {
        let f = parse_filters(&params(&[("to", "2024-01-15")])).unwrap();
        assert_eq!(
            f.date_to.as_deref(),
            Some("2024-01-15T23:59:59.999999+00:00")
        );
        assert!(parse_filters(&params(&[])).is_none());
        assert!(parse_filters(&params(&[("has_link", "true")])).unwrap().has_link);
    }

    #[test]
    fn filtered_view_applies_all_filters() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        assert_eq!(archive.messages.len(), 4, "partial tail dropped");

        let all = archive.ordered_view(None, false);
        assert_eq!(all, vec![0, 1, 2, 3]);

        let by_date = filters(Some("2024-01-01T00:00:00+00:00"), None, false, false, false);
        assert_eq!(*archive.filtered_view(&by_date), vec![2, 3]);

        let att = filters(None, None, true, false, false);
        assert_eq!(*archive.filtered_view(&att), vec![2]);

        let link = filters(None, None, false, true, false);
        assert_eq!(*archive.filtered_view(&link), vec![1]);

        let pin = filters(None, None, false, false, true);
        assert_eq!(*archive.filtered_view(&pin), vec![2]);

        let combo = filters(
            Some("2023-01-01"),
            Some("2023-12-31T23:59:59.999999+00:00"),
            false,
            true,
            false,
        );
        assert_eq!(*archive.filtered_view(&combo), vec![1]);

        let reversed = archive.ordered_view(Some(&att), true);
        assert_eq!(reversed, vec![2]);
    }

    #[test]
    fn page_payload_pages_clamps_and_enriches() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        let page = archive.page_payload(0, 2, None, false);
        assert_eq!(page["total"], json!(4));
        assert_eq!(page["start"], json!(0));
        assert_eq!(page["messages"].as_array().unwrap().len(), 2);
        assert_eq!(page["messages"][0]["seq"], json!(0));
        assert_eq!(page["messages"][0]["id"], json!("100"));

        // negative start counts back from the end
        let tail = archive.page_payload(-1, 10, None, false);
        assert_eq!(tail["start"], json!(3));
        assert_eq!(tail["messages"][0]["id"], json!("103"));

        // out-of-range start clamps to the end (empty page)
        let beyond = archive.page_payload(99, 10, None, false);
        assert_eq!(beyond["start"], json!(4));
        assert_eq!(beyond["messages"].as_array().unwrap().len(), 0);

        // attachment view enrichment: only on-disk attachments appear
        let page = archive.page_payload(2, 1, None, false);
        let atts = page["messages"][0]["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["filename"], json!("photo.png"));
        assert_eq!(atts[0]["url"], json!("/media/0001_102_photo.png"));

        // desc direction
        let desc = archive.page_payload(0, 2, None, true);
        assert_eq!(desc["messages"][0]["id"], json!("103"));
    }

    #[test]
    fn anchor_payload_centers_on_message() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        let payload = archive.anchor_payload("101", 3, None, false).unwrap();
        assert_eq!(payload["anchor_index"], json!(1));
        let ids: Vec<&str> = payload["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["100", "101", "102"]);
        assert!(archive.anchor_payload("999", 3, None, false).is_none());
        // anchor excluded by filters -> None
        let att = filters(None, None, true, false, false);
        assert!(archive.anchor_payload("100", 3, Some(&att), false).is_none());
    }

    #[test]
    fn search_is_case_insensitive_newest_first_and_limited() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        let res = archive.search_payload("hello", None, None, SEARCH_MAX);
        let ids: Vec<&str> = res["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["103", "100"], "most recent first");
        assert_eq!(res["truncated"], json!(false));

        let by_author = archive.search_payload("hello", Some("ALICE"), None, SEARCH_MAX);
        assert_eq!(by_author["results"].as_array().unwrap().len(), 1);

        let truncated = archive.search_payload("hello", None, None, 1);
        assert_eq!(truncated["truncated"], json!(true));
        assert_eq!(truncated["results"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn resolve_payload_returns_snippets() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        let res = archive.resolve_payload(&["100".to_string(), "999".to_string()]);
        assert_eq!(res["100"]["author"], json!("alice"));
        assert_eq!(res["100"]["index"], json!(0));
        assert_eq!(res["100"]["content"], json!("hello world"));
        assert!(res.get("999").is_none());
    }

    #[test]
    fn attachments_and_years() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        let years = archive.attachment_year_counts();
        assert_eq!(years, json!([{ "year": "2024", "count": 1 }]));
        let payload = archive.attachments_payload(0, 60, None);
        assert_eq!(payload["total"], json!(1));
        let item = &payload["attachments"][0];
        assert_eq!(item["message_id"], json!("102"));
        assert_eq!(item["message_index"], json!(2));
        assert_eq!(item["date"], json!("2024-01-15"));
        let filtered = archive.attachments_payload(0, 60, Some("2023"));
        assert_eq!(filtered["total"], json!(0));
    }

    #[test]
    fn stats_counts() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        let s = archive.stats_payload();
        assert_eq!(s["total_messages"], json!(4));
        assert_eq!(s["total_attachments"], json!(1));
        assert_eq!(s["attachment_bytes"], json!(4));
        assert_eq!(s["edited"], json!(1));
        assert_eq!(s["replies"], json!(1));
        assert_eq!(s["pinned"], json!(1));
        assert_eq!(s["with_attachments"], json!(1));
        assert_eq!(s["authors"][0], json!({"name": "alice", "count": 2}));
        assert_eq!(s["months"].as_array().unwrap().len(), 3);
        assert_eq!(s["attachment_types"], json!([{ "ext": "png", "count": 1 }]));
    }

    #[test]
    fn resolve_rejects_traversal_and_serves_files() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        assert!(archive.resolve("0001_102_photo.png").is_some());
        assert!(archive.resolve("../messages.jsonl").is_none());
        assert!(archive.resolve("..").is_none());
        assert!(archive.resolve("sub/dir.png").is_none());
        assert!(archive.resolve("missing.png").is_none());
        assert_eq!(Archive::attachment_mime("x.PNG"), "image/png");
        assert_eq!(Archive::attachment_mime("x.bin"), "application/octet-stream");
    }

    #[test]
    fn find_archives_lists_only_archives() {
        let fx = fixture();
        // a non-archive folder must be skipped
        std::fs::create_dir_all(fx.root.join("not-an-archive")).unwrap();
        let archives = find_archives(&fx.root);
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0]["name"], json!("chat-one"));
        assert_eq!(archives[0]["chat"], json!("Chat One"));
        assert_eq!(archives[0]["channel_id"], json!("111"));
    }

    #[test]
    fn library_loads_selects_and_rejects_traversal() {
        let fx = fixture();
        let library = Library::new(fx.root.clone());
        assert!(library.load("..").is_none());
        assert!(library.load("nope").is_none());
        let archive = library.select("chat-one").expect("selectable");
        assert_eq!(archive.messages.len(), 4);
        assert_eq!(library.selected_name().as_deref(), Some("chat-one"));
        assert!(library.selected().is_some());
    }

    #[test]
    fn host_guard_rules() {
        assert!(host_allowed("localhost:8765"));
        assert!(host_allowed("127.0.0.1:8765"));
        assert!(host_allowed("[::1]:8765"));
        assert!(host_allowed("127.0.0.1"));
        assert!(!host_allowed("evil.example.com:8765"));
        assert!(!host_allowed(""));
    }

    #[test]
    fn range_parsing() {
        assert!(matches!(parse_range("bytes=0-99", 1000), ByteRange::Partial(0, 99)));
        assert!(matches!(parse_range("bytes=500-", 1000), ByteRange::Partial(500, 999)));
        assert!(matches!(parse_range("bytes=-100", 1000), ByteRange::Partial(900, 999)));
        assert!(matches!(parse_range("bytes=2000-", 1000), ByteRange::Unsatisfiable));
        assert!(matches!(parse_range("bytes=", 1000), ByteRange::Full));
        assert!(matches!(parse_range("garbage", 1000), ByteRange::Full));
    }

    #[test]
    fn query_map_first_value_wins_and_decodes() {
        let q = query_map(Some("q=hello+world&archive=a%20b&ids=1,2&ids=3"));
        assert_eq!(q.get("q").unwrap(), "hello world");
        assert_eq!(q.get("archive").unwrap(), "a b");
        assert_eq!(q.get("ids").unwrap(), "1,2");
        assert!(query_map(None).is_empty());
    }
}
