//! Platform API client: session setup, channel/guild listing, paginated
//! history with retries, deletion with rate limiting, and streaming
//! attachment downloads. A direct port of V1's message_guillotine/api.py.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use base64::Engine;
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

pub const API_BASE: &str = "https://discord.com/api/v10";
/// 24 messages per minute = ~2.5s each; 2.6s for safety.
pub const RATE_LIMIT_DELAY: Duration = Duration::from_millis(2600);
/// Politeness delay between CDN downloads (the CDN is not API rate limited).
pub const ATTACHMENT_DELAY: Duration = Duration::from_millis(50);
pub const FETCH_RETRIES: u32 = 5;
pub const FETCH_RETRY_DELAY: Duration = Duration::from_secs(5);
pub const DELETE_RETRIES: u32 = 3;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Message IDs encode (unix_ms - this) << 22.
pub const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
/// System messages (calls, pin notifications) can never be deleted via the
/// API; the server answers 403 with this code.
pub const UNDELETABLE_MESSAGE_CODES: [i64; 1] = [50021];
/// Classic system message types, filtered out locally before any delete
/// attempt so a stack of them never burns rate-limit budget.
pub const SYSTEM_MESSAGE_TYPES: [u64; 7] = [1, 2, 3, 4, 5, 6, 7];

/// A complete, real browser UA; the platform rejects writes from clients it
/// cannot fingerprint.
pub const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

fn super_properties_header() -> HeaderValue {
    let props = serde_json::json!({
        "os": "Windows",
        "browser": "Chrome",
        "device": "",
        "system_locale": "en-US",
        "browser_user_agent": BROWSER_USER_AGENT,
        "browser_version": "126.0.0.0",
        "os_version": "10",
        "referrer": "",
        "referring_domain": "",
        "referrer_current": "",
        "referring_domain_current": "",
        "release_channel": "stable",
        "client_build_number": 275457,
        "client_event_source": null,
        "design_id": 0,
    });
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_string(&props).unwrap().as_bytes());
    HeaderValue::from_str(&encoded).unwrap()
}

/// A page of channel history could not be fetched after retrying.
#[derive(Debug, thiserror::Error)]
#[error("history fetch failed: {0}")]
pub struct HistoryFetchError(pub String);

/// The lowest message id whose timestamp is at or after `dt`. Message ids
/// embed their creation time, so a datetime can bound a query by id.
pub fn snowflake_for(dt: DateTime<Utc>) -> String {
    let ms = dt.timestamp_millis();
    // Saturate pre-epoch dates at id 0 instead of wrapping to a huge id.
    if ms <= DISCORD_EPOCH_MS {
        return "0".to_string();
    }
    (((ms - DISCORD_EPOCH_MS) << 22) as u64).to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub discriminator: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    pub id: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub timestamp: String,
    pub author: User,
    #[serde(default)]
    pub content: String,
    #[serde(default = "default_type")]
    pub r#type: u64,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
}

fn default_type() -> u64 {
    0
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageReference {
    #[serde(default)]
    pub message_id: Option<String>,
}

impl Message {
    /// Seconds since the epoch for the message timestamp (ISO, UTC).
    pub fn epoch(&self) -> anyhow::Result<f64> {
        let dt = DateTime::parse_from_rfc3339(&self.timestamp)?;
        Ok(dt.timestamp() as f64 + dt.timestamp_subsec_millis() as f64 / 1000.0)
    }

    pub fn is_system(&self) -> bool {
        SYSTEM_MESSAGE_TYPES.contains(&self.r#type)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DmChannel {
    pub id: String,
    #[serde(default)]
    pub recipients: Vec<User>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildChannel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub r#type: u64,
}

/// Outcome of one delete attempt with bounded retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The message is gone; carries the extra seconds the server asked for
    /// before the next delete when the rate bucket is exhausted.
    Deleted(Duration),
    /// A system message the API can never delete; the run is unaffected.
    Skipped,
    /// An unexpected error on this message; the run continues.
    Failed,
    /// 401/403: the token was rejected or this account cannot delete here;
    /// every further delete would fail the same way.
    Auth,
    /// Still 429 after DELETE_RETRIES attempts.
    RateLimited,
    /// The platform was unreachable for DELETE_RETRIES attempts.
    Network,
}

pub struct Client {
    http: reqwest::Client,
    /// Separate client for CDN attachment downloads: keeps TLS connections
    /// alive across many files and keeps the auth token off the CDN.
    download: reqwest::Client,
    pub current_user: Option<User>,
}

impl Client {
    pub fn new(token: &str) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(token).context("token contains invalid header characters")?,
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(BROWSER_USER_AGENT),
        );
        headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("en-US,en;q=0.9"),
        );
        headers.insert("X-Discord-Locale", HeaderValue::from_static("en-US"));
        headers.insert("X-Super-Properties", super_properties_header());

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()?;

        let download = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .default_headers({
                let mut h = HeaderMap::new();
                h.insert(
                    reqwest::header::USER_AGENT,
                    HeaderValue::from_static(BROWSER_USER_AGENT),
                );
                h
            })
            .read_timeout(READ_TIMEOUT)
            .build()?;

        Ok(Client {
            http,
            download,
            current_user: None,
        })
    }

    pub async fn get_current_user(&mut self) -> anyhow::Result<Option<User>> {
        let resp = self
            .http
            .get(format!("{API_BASE}/users/@me"))
            .send()
            .await?;
        if resp.status() == 200 {
            let user: User = resp.json().await?;
            tracing::info!(username = %user.username, id = %user.id, "logged in");
            self.current_user = Some(user.clone());
            Ok(Some(user))
        } else {
            Ok(None)
        }
    }

    pub async fn get_dm_channels(&self) -> anyhow::Result<Vec<DmChannel>> {
        let resp = self
            .http
            .get(format!("{API_BASE}/users/@me/channels"))
            .send()
            .await?;
        Ok(resp.json().await?)
    }

    pub async fn get_guilds(&self) -> anyhow::Result<Vec<Guild>> {
        let resp = self
            .http
            .get(format!("{API_BASE}/users/@me/guilds"))
            .send()
            .await?;
        Ok(resp.json().await?)
    }

    /// Text-like channels of a guild (type 0 text, 5 announcement, 10/11/12 threads).
    pub async fn get_guild_channels(&self, guild_id: &str) -> anyhow::Result<Vec<GuildChannel>> {
        let resp = self
            .http
            .get(format!("{API_BASE}/guilds/{guild_id}/channels"))
            .send()
            .await?;
        let channels: Vec<GuildChannel> = resp.json().await?;
        Ok(channels
            .into_iter()
            .filter(|c| matches!(c.r#type, 0 | 5 | 10 | 11 | 12))
            .collect())
    }

    /// All users you have DMs with (bots excluded), keyed by user id.
    pub fn dm_users(&self, dm_channels: &[DmChannel]) -> Vec<(User, String)> {
        let me = self.current_user.as_ref().map(|u| u.id.as_str());
        let mut users: std::collections::BTreeMap<String, (User, String)> =
            std::collections::BTreeMap::new();
        for channel in dm_channels {
            for recipient in &channel.recipients {
                if recipient.bot {
                    continue;
                }
                if Some(recipient.id.as_str()) == me {
                    continue;
                }
                users.insert(
                    recipient.id.clone(),
                    (recipient.clone(), channel.id.clone()),
                );
            }
        }
        users.into_values().collect()
    }

    /// Resolve a DM user id to its channel id via the DM channel list.
    pub async fn dm_channel_for_user(&self, user_id: &str) -> anyhow::Result<Option<String>> {
        for channel in self.get_dm_channels().await? {
            if channel.recipients.iter().any(|r| r.id == user_id && !r.bot) {
                return Ok(Some(channel.id));
            }
        }
        Ok(None)
    }

    /// One page of channel history, newest-first, starting older than
    /// `before_id` (the whole channel when None). Retries rate limits and
    /// transient failures; raises HistoryFetchError when attempts run out.
    pub async fn history_page_before(
        &self,
        channel_id: &str,
        before_id: Option<&str>,
    ) -> Result<Vec<Message>, HistoryFetchError> {
        let mut query = vec![("limit".to_string(), "100".to_string())];
        if let Some(b) = before_id {
            query.push(("before".to_string(), b.to_string()));
        }
        let resp = self
            .fetch_page_with_retries(
                &format!("{API_BASE}/channels/{channel_id}/messages"),
                &query,
            )
            .await?;
        let mut messages: Vec<Message> = resp
            .json()
            .await
            .map_err(|e| HistoryFetchError(e.to_string()))?;
        messages.sort_by_key(|m| std::cmp::Reverse(m.id.parse::<u128>().unwrap_or(0)));
        Ok(messages)
    }

    /// One page of channel history, oldest-first, starting after `after_id`.
    pub async fn history_page_after(
        &self,
        channel_id: &str,
        after_id: &str,
    ) -> Result<Vec<Message>, HistoryFetchError> {
        let query = vec![
            ("limit".to_string(), "100".to_string()),
            ("after".to_string(), after_id.to_string()),
        ];
        let resp = self
            .fetch_page_with_retries(
                &format!("{API_BASE}/channels/{channel_id}/messages"),
                &query,
            )
            .await?;
        let mut messages: Vec<Message> = resp
            .json()
            .await
            .map_err(|e| HistoryFetchError(e.to_string()))?;
        messages.sort_by_key(|m| m.id.parse::<u128>().unwrap_or(0));
        Ok(messages)
    }

    async fn fetch_page_with_retries(
        &self,
        url: &str,
        query: &[(String, String)],
    ) -> Result<reqwest::Response, HistoryFetchError> {
        let mut last_error = "unknown error".to_string();
        for attempt in 1..=FETCH_RETRIES {
            match self.http.get(url).query(query).send().await {
                Err(exc) => last_error = format!("network error: {exc}"),
                Ok(resp) => {
                    let status = resp.status();
                    if status == 200 {
                        return Ok(resp);
                    }
                    if status == 429 {
                        let retry_after = retry_after(&resp, FETCH_RETRY_DELAY);
                        last_error = "rate limited (429)".to_string();
                        tracing::warn!(?retry_after, "rate limited; retrying");
                        tokio::time::sleep(retry_after).await;
                        continue;
                    }
                    last_error = format!("HTTP {status}");
                    if matches!(status.as_u16(), 401 | 403 | 404) {
                        break; // auth or access errors will not succeed on retry
                    }
                }
            }
            if attempt < FETCH_RETRIES {
                tracing::warn!(%last_error, attempt, "page fetch failed; retrying");
                tokio::time::sleep(FETCH_RETRY_DELAY).await;
            }
        }
        Err(HistoryFetchError(last_error))
    }

    /// Delete one message, retrying when rate limited. A repeat delete on an
    /// already-deleted message returns 404, which after a network error
    /// counts as deleted (the first attempt went through, its reply just
    /// never arrived).
    pub async fn delete_message_with_retries(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> DeleteOutcome {
        let mut last_failure = DeleteOutcome::RateLimited;
        let mut unconfirmed = false;
        for attempt in 1..=DELETE_RETRIES {
            let resp = match self
                .http
                .delete(format!(
                    "{API_BASE}/channels/{channel_id}/messages/{message_id}"
                ))
                .send()
                .await
            {
                Ok(r) => r,
                Err(exc) => {
                    last_failure = DeleteOutcome::Network;
                    unconfirmed = true;
                    tracing::warn!(%exc, attempt, "network error on delete; retrying");
                    tokio::time::sleep(FETCH_RETRY_DELAY).await;
                    continue;
                }
            };
            let status = resp.status();
            if status == 404 && unconfirmed {
                return DeleteOutcome::Deleted(Duration::ZERO);
            }
            unconfirmed = false;
            if status == 200 || status == 204 {
                let mut wait = Duration::ZERO;
                if resp
                    .headers()
                    .get("X-RateLimit-Remaining")
                    .and_then(|v| v.to_str().ok())
                    == Some("0")
                {
                    if let Some(v) = resp
                        .headers()
                        .get("X-RateLimit-Reset-After")
                        .and_then(|v| v.to_str().ok())
                    {
                        if let Ok(secs) = v.parse::<f64>() {
                            wait = Duration::from_secs_f64(secs);
                        }
                    }
                }
                return DeleteOutcome::Deleted(wait);
            }
            if status == 429 {
                last_failure = DeleteOutcome::RateLimited;
                let retry_after = retry_after(&resp, RATE_LIMIT_DELAY);
                tracing::warn!(?retry_after, attempt, "rate limited on delete; retrying");
                tokio::time::sleep(retry_after).await;
                continue;
            }
            if status == 401 || status == 403 {
                if status == 403
                    && error_code(resp)
                        .await
                        .is_some_and(|c| UNDELETABLE_MESSAGE_CODES.contains(&c))
                {
                    return DeleteOutcome::Skipped;
                }
                let reason = if status == 401 {
                    "the token was rejected - re-authenticate"
                } else {
                    "this account is not allowed to delete messages in this channel"
                };
                tracing::error!(status = status.as_u16(), %reason, "stopping deletion run");
                return DeleteOutcome::Auth;
            }
            return DeleteOutcome::Failed;
        }
        if last_failure == DeleteOutcome::Network {
            tracing::error!("network errors persisted through {DELETE_RETRIES} attempts; stopping");
        } else {
            tracing::error!("still rate limited after {DELETE_RETRIES} attempts; stopping");
        }
        last_failure
    }

    /// Stream an attachment to dest_path via a .part file, renamed on
    /// completion. Returns bytes written, or None on failure.
    pub async fn download_attachment(&self, url: &str, dest: &Path) -> Option<u64> {
        let tmp = dest.with_extension(format!(
            "{}.part",
            dest.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        let resp = self.download.get(url).send().await.ok()?;
        if resp.status() != 200 {
            return None;
        }
        let mut file = tokio::fs::File::create(&tmp).await.ok()?;
        use tokio::io::AsyncWriteExt;
        let mut size = 0u64;
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        const MAX_ATTACHMENT_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB guard
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            size += chunk.len() as u64;
            if size > MAX_ATTACHMENT_BYTES {
                let _ = tokio::fs::remove_file(&tmp).await;
                return None;
            }
            file.write_all(&chunk).await.ok()?;
        }
        file.flush().await.ok()?;
        drop(file);
        tokio::fs::rename(&tmp, dest).await.ok()?;
        Some(size)
    }
}

/// Seconds to wait from a Retry-After header, or fallback when absent or
/// malformed. Never below 1s.
fn retry_after(resp: &reqwest::Response, fallback: Duration) -> Duration {
    let secs = resp
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(fallback.as_secs_f64());
    Duration::from_secs_f64(secs.max(1.0))
}

async fn error_code(resp: reqwest::Response) -> Option<i64> {
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("code").and_then(|c| c.as_i64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_encodes_timestamp() {
        // 2024-01-01T00:00:00Z = 1704067200000 ms
        let dt = DateTime::from_timestamp_millis(1_704_067_200_000).unwrap();
        let id = snowflake_for(dt);
        let parsed: u64 = id.parse().unwrap();
        let ts_ms = (parsed >> 22) + DISCORD_EPOCH_MS as u64;
        assert_eq!(ts_ms, 1_704_067_200_000);
    }

    #[test]
    fn system_types_detected() {
        let mut msg: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "timestamp": "2024-01-01T00:00:00.000+00:00",
            "author": {"id": "2", "username": "u"}
        }))
        .unwrap();
        assert!(!msg.is_system());
        msg.r#type = 3; // call
        assert!(msg.is_system());
    }
}
