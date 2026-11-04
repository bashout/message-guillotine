//! HTTP layer: query-string helpers, the host guard, security headers, and
//! the JSON API handlers.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{middleware, Json};
use serde_json::{json, Value};

use super::assets::{APP_JS, INDEX_HTML, STYLE_CSS};
use super::library::{find_archives, Archive, Library};
use super::{parse_filters, GALLERY_DEFAULT, PAGE_DEFAULT, PAGE_MAX, SEARCH_MAX};

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

type ApiError = (StatusCode, Json<Value>);
type ApiResult = Result<Json<Value>, ApiError>;

pub fn json_err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(json!({ "error": msg })))
}

/// Minimal query-string parser (first value wins, like the V1 handler).
pub fn query_map(raw: Option<&str>) -> HashMap<String, String> {
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

pub fn percent_decode(s: &str) -> String {
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

pub fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn q_int(q: &HashMap<String, String>, name: &str, default: i64) -> Result<i64, ApiError> {
    match q.get(name) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<i64>()
            .map_err(|_| json_err(StatusCode::BAD_REQUEST, &format!("invalid {name}: {raw:?}"))),
    }
}

/// The archive for a request: the ?archive= pin when given, else the shared
/// selection.
pub fn request_archive(
    library: &Library,
    q: &HashMap<String, String>,
) -> Result<Arc<Archive>, ApiError> {
    if let Some(name) = q.get("archive") {
        if name.is_empty() {
            return Err(json_err(
                StatusCode::BAD_REQUEST,
                "archive parameter required",
            ));
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

/// Extract the hostname from a Host header value: lowercased, brackets
/// stripped, port removed (only when it is numeric). None when malformed.
fn host_name(host: &str) -> Option<String> {
    let host = host.trim().to_lowercase();
    if host.is_empty() {
        return None;
    }
    let name = if host.starts_with('[') {
        let end = host.find(']')?;
        host[1..end].to_string()
    } else if host
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        host.rsplit_once(':').unwrap().0.to_string()
    } else {
        host
    };
    Some(name)
}

/// Reject non-local Host headers (DNS-rebinding protection). Only
/// "localhost" and loopback IP literals (127.0.0.0/8, ::1) are allowed —
/// any other IP literal or hostname is rejected.
pub fn host_allowed(host: &str) -> bool {
    let Some(name) = host_name(host) else {
        return false;
    };
    if name == "localhost" {
        return true;
    }
    name.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// State for the host-guard middleware: the address the viewer is actually
/// bound to, and whether the user explicitly opted into a non-loopback bind
/// with --expose.
#[derive(Clone)]
pub struct HostGuard {
    bound: Option<std::net::IpAddr>,
    exposed: bool,
}

impl HostGuard {
    pub fn new(bound: std::net::IpAddr, exposed: bool) -> Self {
        HostGuard {
            bound: Some(bound),
            exposed,
        }
    }

    pub fn allows(&self, host: &str) -> bool {
        if host_allowed(host) {
            return true;
        }
        let Some(name) = host_name(host) else {
            return false;
        };
        let Ok(ip) = name.parse::<std::net::IpAddr>() else {
            return false;
        };
        if self.bound == Some(ip) {
            return true;
        }
        // In --expose mode a browser on the LAN addresses the viewer by its
        // interface IP; hostname rebinding stays blocked by host_allowed.
        self.exposed && !ip.is_loopback()
    }
}

/// Baseline security headers on every viewer response.
pub async fn security_headers(req: axum::extract::Request, next: middleware::Next) -> Response {
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

pub async fn host_guard(
    State(guard): State<HostGuard>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if guard.allows(host) {
        next.run(req).await
    } else {
        json_err(StatusCode::FORBIDDEN, "forbidden host").into_response()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn index_html() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
        .into_response()
}

pub async fn app_js() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
        .into_response()
}

pub async fn style_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

pub async fn api_archives(State(library): State<Arc<Library>>) -> Json<Value> {
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

pub async fn api_select(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
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

pub async fn api_meta(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    Ok(Json(archive.meta_payload()))
}

pub async fn api_messages(
    State(library): State<Arc<Library>>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
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
    Ok(Json(archive.page_payload(
        start,
        count,
        filters.as_ref(),
        reverse,
    )))
}

pub async fn api_resolve(
    State(library): State<Arc<Library>>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    let ids: Vec<String> = q
        .get("ids")
        .map(|s| s.split(',').take(100).map(|s| s.to_string()).collect())
        .unwrap_or_default();
    Ok(Json(archive.resolve_payload(&ids)))
}

pub async fn api_search(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    let query = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    if query.is_empty() {
        return Ok(Json(
            json!({ "query": "", "results": [], "truncated": false }),
        ));
    }
    let author = q.get("author").map(|s| s.as_str());
    let filters = parse_filters(&q);
    Ok(Json(archive.search_payload(
        &query,
        author,
        filters.as_ref(),
        SEARCH_MAX,
    )))
}

pub async fn api_attachment_years(
    State(library): State<Arc<Library>>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    Ok(Json(json!({ "years": archive.attachment_year_counts() })))
}

pub async fn api_attachments(
    State(library): State<Arc<Library>>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    let count = q_int(&q, "count", GALLERY_DEFAULT as i64)?.clamp(1, PAGE_MAX as i64) as usize;
    let start = q_int(&q, "start", 0)?.max(0) as usize;
    let year = q.get("year").map(|s| s.as_str());
    Ok(Json(archive.attachments_payload(start, count, year)))
}

pub async fn api_stats(State(library): State<Arc<Library>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = query_map(raw.as_deref());
    let archive = request_archive(&library, &q)?;
    Ok(Json(archive.stats_payload()))
}
