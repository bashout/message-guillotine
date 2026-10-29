//! Attachment file serving with single-range support (so <video>/<audio> can
//! seek) and inline-vs-download disposition rules.

use axum::body::Body;
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use super::api::{json_err, query_map, request_archive};
use super::library::{Archive, Library};

pub enum ByteRange {
    Full,
    Partial(u64, u64), // start, end inclusive
    Unsatisfiable,
}

pub fn parse_range(header_value: &str, size: u64) -> ByteRange {
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
pub async fn media(
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
