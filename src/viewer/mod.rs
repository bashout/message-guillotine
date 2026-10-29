//! V2 local archive viewer: serves an archival folder over localhost HTTP
//! with an embedded single-page UI (archive selector, paged messages, search,
//! date/attachment/link/pinned filters, reply threading, media gallery, stats).
//!
//! The on-disk format is the one documented in `archive_format`; this module
//! only reads it. The JSON API mirrors the V1 Python viewer so both frontends
//! could be swapped against either server.
//!
//! Split: this file holds the filter parsing, the public constants, and the
//! server entry point; `library.rs` loads archives, `api.rs` serves the JSON
//! API, `media.rs` serves attachment files with range support, and `assets.rs`
//! embeds the single-page UI.

pub mod api;
pub mod assets;
pub mod library;
pub mod media;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{middleware, Router};
use serde_json::Value;

pub use api::{
    api_archives, api_attachments, api_attachment_years, api_messages, api_meta, api_resolve,
    api_search, api_select, api_stats, app_js, host_guard, index_html, security_headers,
    style_css,
};
pub use library::{find_archives, Library};
pub use media::media;

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

#[cfg(test)]
mod tests {
    use super::api::{host_allowed, query_map};
    use super::library::{Archive, Library};
    use super::media::{parse_range, ByteRange};
    use super::*;
    use serde_json::{json, Value};
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
    #[test]
    fn range_parsing_multi_and_edge_cases() {
        // multi-range requests are not supported: the parser treats
        // "0-1,5-6" as "0-" (open-ended), never as two ranges
        assert!(matches!(
            parse_range("bytes=0-1,5-6", 100),
            ByteRange::Partial(0, 99)
        ));
        // end beyond size is clamped to the last byte
        assert!(matches!(parse_range("bytes=10-9999", 100), ByteRange::Partial(10, 99)));
        // reversed range is unsatisfiable
        assert!(matches!(parse_range("bytes=50-10", 100), ByteRange::Unsatisfiable));
        // suffix larger than the file -> whole file
        assert!(matches!(parse_range("bytes=-9999", 100), ByteRange::Partial(0, 99)));
        // suffix of zero -> unsatisfiable
        assert!(matches!(parse_range("bytes=-0", 100), ByteRange::Unsatisfiable));
        // non-numeric -> full body
        assert!(matches!(parse_range("bytes=abc-def", 100), ByteRange::Full));
        // empty size -> always unsatisfiable
        assert!(matches!(parse_range("bytes=0-", 0), ByteRange::Unsatisfiable));
        assert!(matches!(parse_range("bytes=-5", 0), ByteRange::Unsatisfiable));
    }

    #[test]
    fn host_guard_edge_cases() {
        assert!(host_allowed("localhost"));
        assert!(host_allowed("LOCALHOST:8765"));
        assert!(host_allowed("[::1]"));
        assert!(host_allowed("192.168.1.10:8765"));
        assert!(!host_allowed("127.0.0.1:abc"), "non-numeric port is not stripped");
        assert!(!host_allowed("localhost.evil.com"));
        assert!(!host_allowed("[::1"), "unterminated bracket");
        assert!(!host_allowed("0.0.0.0.evil.com:80"));
    }

    #[test]
    fn parse_filters_flag_combinations() {
        // has_attachment / pinned only activate on the literal "true"
        assert!(parse_filters(&params(&[("has_attachment", "1")])).is_none());
        assert!(parse_filters(&params(&[("pinned", "false")])).is_none());
        let f = parse_filters(&params(&[("has_attachment", "true"), ("pinned", "true")])).unwrap();
        assert!(f.has_attachment && f.pinned && !f.has_link);
        // full ISO 'from' is normalized to UTC before comparison
        let f = parse_filters(&params(&[("from", "2024-01-15T14:30:00+02:00")])).unwrap();
        assert_eq!(f.date_from.as_deref(), Some("2024-01-15T12:30:00+00:00"));
        // bare 'from' date passes through unchanged (start of day semantics)
        let f = parse_filters(&params(&[("from", "2024-01-15")])).unwrap();
        assert_eq!(f.date_from.as_deref(), Some("2024-01-15"));
    }

    #[test]
    fn library_lru_evicts_oldest() {
        let fx = fixture();
        // second archive
        let other = fx.root.join("chat-two");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("meta.json"), r#"{"chat": "Two", "channel_id": "222"}"#).unwrap();
        std::fs::write(other.join("messages.jsonl"), "").unwrap();
        let library = Library::new(fx.root.clone());
        let a = library.load("chat-one").unwrap();
        let b = library.load("chat-two").unwrap();
        // cache holds LIBRARY_CACHE_MAX (2): loading a third evicts chat-one
        let third = fx.root.join("chat-three");
        std::fs::create_dir_all(&third).unwrap();
        std::fs::write(third.join("meta.json"), r#"{"chat": "Three", "channel_id": "333"}"#).unwrap();
        std::fs::write(third.join("messages.jsonl"), "").unwrap();
        let _c = library.load("chat-three").unwrap();
        // chat-one was evicted: its Arc is only held by our local variable
        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&b), 2, "chat-two still cached");
        // loading chat-one again reloads from disk (works, not an error)
        assert!(library.load("chat-one").is_some());
        // selection is independent of the cache
        library.select("chat-two");
        assert_eq!(library.selected_name().as_deref(), Some("chat-two"));
    }

    #[test]
    fn attachments_payload_paging_clamps() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        // start beyond total -> empty page, clamped start
        let p = archive.attachments_payload(99, 60, None);
        assert_eq!(p["total"], json!(1));
        assert_eq!(p["start"], json!(1));
        assert_eq!(p["attachments"].as_array().unwrap().len(), 0);
        // count larger than remaining -> only what's left
        let p = archive.attachments_payload(0, 60, None);
        assert_eq!(p["attachments"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn anchor_payload_reverse_direction() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        // desc view: positions count from the newest; anchor 101 is position 2 of 4
        let p = archive.anchor_payload("101", 2, None, true).unwrap();
        assert_eq!(p["anchor_index"], json!(2));
        let ids: Vec<&str> = p["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
        // window centered on position 2: start = 2 - 1 = 1 -> [102, 101] in desc order
        assert_eq!(ids, vec!["102", "101"]);
    }

    #[test]
    fn meta_payload_reports_bounds() {
        let fx = fixture();
        let archive = Archive::load(&fx.archive_dir);
        let m = archive.meta_payload();
        assert_eq!(m["chat"], json!("Chat One"));
        assert_eq!(m["channel_id"], json!("111"));
        assert_eq!(m["total_messages"], json!(4));
        assert_eq!(m["total_attachments"], json!(1));
        assert_eq!(m["first_timestamp"], json!("2023-05-01T10:00:00.000000+00:00"));
        assert_eq!(m["last_timestamp"], json!("2024-06-20T08:00:00.000000+00:00"));
    }

}
