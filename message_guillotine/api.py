"""Platform API client for the deleter and archiver.

MessageGuillotine wraps the platform's unofficial user API: session setup,
channel/guild listing, paginated history with retries, deletion with rate
limiting, and streaming attachment downloads."""


import base64
import json
import time

import requests


API_BASE = "https://discord.com/api/v10"  # Platform API endpoint
RATE_LIMIT_DELAY = 2.6  # 24 messages per minute = ~2.5s each, use 2.6s for safety
ATTACHMENT_DELAY = 0.05  # Politeness delay between CDN downloads (CDN is not API rate limited)
FETCH_RETRIES = 5  # attempts per page before the archive run is marked incomplete
FETCH_RETRY_DELAY = 5  # seconds to wait between retries of a failed page fetch
DELETE_RETRIES = 3  # attempts per message when a delete is rate limited
REQUEST_TIMEOUT = (10, 30)  # (connect, read) seconds per HTTP request; no request may hang forever
DISCORD_EPOCH_MS = 1420070400000  # message IDs encode (unix_ms - this) << 22
UNDELETABLE_MESSAGE_CODES = {50021}  # system messages (calls, pin notifications) can never be deleted via the API
# Classic system message types: RECIPIENT_ADD/REMOVE (1/2), CALL (3),
# CHANNEL_NAME_CHANGE (4), CHANNEL_ICON_CHANGE (5), CHANNEL_PINNED_MESSAGE (6),
# GUILD_MEMBER_JOIN (7). The platform rejects deleting any of them (code
# 50021), so deletion runs skip them locally — no request is sent, and a
# stack of them at the top of a chat cannot burn the delete bucket into 429s.
# Other system-ish types stay covered by UNDELETABLE_MESSAGE_CODES server-side.
SYSTEM_MESSAGE_TYPES = {1, 2, 3, 4, 5, 6, 7}

BROWSER_USER_AGENT = (  # A complete, real browser UA; the platform rejects writes from clients it cannot fingerprint
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")

# The web client sends these on every request; writes (deletes especially)
# get 403s without them even when the account can delete via the UI.
SUPER_PROPERTIES = {
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
    "client_event_source": None,
    "design_id": 0,
}


class HistoryFetchError(Exception):
    """A page of channel history could not be fetched after retrying.

    Raised by iter_channel_history so callers can record that the archive
    is incomplete instead of treating an early stop as a finished run."""


def _error_detail(resp):
    """Extract the platform's own error message from an error response,
    so an abort can say why, not just that it failed."""
    try:
        body = resp.json()
        message = body.get("message")
        code = body.get("code")
    except (ValueError, AttributeError):
        message, code = None, None
    if message:
        return f"{message} (code {code})" if code is not None else message
    text = (getattr(resp, "text", "") or "").strip()
    if text:
        return text[:200]
    return ""


def _error_code(resp):
    """The platform's numeric error code from an error response, or None."""
    try:
        return resp.json().get("code")
    except (ValueError, AttributeError):
        return None


def snowflake_for(dt):
    """The lowest message ID whose timestamp is at or after dt. Message IDs
    embed their creation time, so a datetime can bound a query by ID."""
    return str((int(dt.timestamp() * 1000) - DISCORD_EPOCH_MS) << 22)


class MessageGuillotine:
    def __init__(self, token):
        self.token = token
        self.session = requests.Session()
        self.session.headers.update({
            "Authorization": token,
            "User-Agent": BROWSER_USER_AGENT,
            "Accept-Language": "en-US,en;q=0.9",
            "X-Discord-Locale": "en-US",
            "X-Super-Properties": base64.b64encode(
                json.dumps(SUPER_PROPERTIES, separators=(",", ":")).encode()).decode(),
        })
        self.current_user = None
        self.dm_channels = []
        self.guilds = []
        
    def get_current_user(self):
        """Fetch current user information."""
        resp = self.session.get(f"{API_BASE}/users/@me", timeout=REQUEST_TIMEOUT)
        if resp.status_code == 200:
            self.current_user = resp.json()
            print(f"Logged in as: {self.current_user['username']}#{self.current_user['discriminator']} (ID: {self.current_user['id']})")
            return True
        else:
            print(f"Failed to get user: {resp.status_code} - {resp.text}")
            return False
    
    def get_dm_channels(self):
        """Get all DM channels."""
        resp = self.session.get(f"{API_BASE}/users/@me/channels", timeout=REQUEST_TIMEOUT)
        if resp.status_code == 200:
            self.dm_channels = resp.json()
            return True
        else:
            print(f"Failed to get DM channels: {resp.status_code}")
            return False
    
    def get_guilds(self):
        """Get all guilds (servers) the user is in."""
        resp = self.session.get(f"{API_BASE}/users/@me/guilds", timeout=REQUEST_TIMEOUT)
        if resp.status_code == 200:
            self.guilds = resp.json()
            return True
        else:
            print(f"Failed to get guilds: {resp.status_code}")
            return False
    
    def get_guild_channels(self, guild_id):
        """Get all text channels in a guild."""
        resp = self.session.get(f"{API_BASE}/guilds/{guild_id}/channels", timeout=REQUEST_TIMEOUT)
        if resp.status_code == 200:
            return [c for c in resp.json() if c.get('type') in (0, 5, 10, 11, 12)]  # Text, News, etc.
        else:
            print(f"Failed to get channels: {resp.status_code}")
            return []
    
    def get_all_dm_users(self):
        """Get list of all users you have DMs with (bots excluded)."""
        users = {}
        for channel in self.dm_channels:
            if 'recipients' in channel:
                last_message_id = channel.get('last_message_id')
                for recipient in channel['recipients']:
                    if recipient.get('bot'):
                        continue
                    user_id = recipient['id']
                    if user_id != self.current_user['id']:
                        users[user_id] = {
                            'id': user_id,
                            'username': recipient.get('username', 'Unknown'),
                            'discriminator': recipient.get('discriminator', '0000'),
                            'global_name': recipient.get('global_name', ''),
                            'channel_id': channel['id'],
                            'last_message_id': last_message_id
                        }
        return list(users.values())

    def count_my_messages(self, channel_id, author_id, min_id=None, max_id=None):
        """Count of the author's messages with IDs in [min_id, max_id]
        (either bound may be None = no limit), from the search endpoint's
        total_results. One request on the search bucket — it never touches
        the history or delete buckets, so it cannot slow the scan. Returns
        None when search is unavailable (disabled, not indexed) or the call
        fails: the count only feeds a time estimate, and callers just hide
        the estimate."""
        params = {"author_id": str(author_id)}
        if min_id is not None:
            params["min_id"] = str(min_id)
        if max_id is not None:
            params["max_id"] = str(max_id)
        try:
            resp = self.session.get(
                f"{API_BASE}/channels/{channel_id}/messages/search",
                params=params, timeout=REQUEST_TIMEOUT)
        except requests.RequestException:
            return None
        if resp.status_code != 200:
            return None
        try:
            total = resp.json().get("total_results")
        except (ValueError, AttributeError):
            return None
        return total if isinstance(total, int) and total >= 0 else None
    
    def iter_channel_history_before(self, channel_id, before_id=None, limit=100):
        """Yield messages newest-first, one fetched page at a time, starting
        older than before_id (the whole channel when it is None). Used by
        deletion so huge channels stream with progress instead of buffering,
        and a run can stop at a time-range boundary instead of fetching the
        full history. Raises HistoryFetchError when a page cannot be fetched
        after retrying."""
        cursor = str(before_id) if before_id else None
        while True:
            params = {"limit": limit}
            if cursor:
                params["before"] = cursor

            resp = self._fetch_page_with_retries(
                f"{API_BASE}/channels/{channel_id}/messages", params)

            messages = resp.json()
            if not messages:
                return

            # `before` pages come back newest-first; enforce just in case
            messages.sort(key=lambda m: int(m['id']), reverse=True)
            for msg in messages:
                yield msg

            cursor = messages[-1]['id']
            if len(messages) < limit:
                return
            time.sleep(0.5)

    def iter_channel_history(self, channel_id, after_id="0", limit=100):
        """Yield messages oldest-first, one fetched page at a time, starting after after_id.
        Used by archive mode so huge chats stream without buffering the full history.
        Raises HistoryFetchError when a page cannot be fetched after retrying."""
        cursor = str(after_id)
        while True:
            params = {"limit": limit, "after": cursor}
            resp = self._fetch_page_with_retries(
                f"{API_BASE}/channels/{channel_id}/messages", params)

            messages = resp.json()
            if not messages:
                return

            # `after` pages come back oldest-first; enforce just in case
            messages.sort(key=lambda m: int(m['id']))
            for msg in messages:
                yield msg

            cursor = messages[-1]['id']
            time.sleep(0.5)

    def _fetch_page_with_retries(self, url, params):
        """GET url, retrying rate limits and transient failures. Raises
        HistoryFetchError once FETCH_RETRIES attempts are exhausted, or
        immediately for statuses that will not succeed on retry."""
        last_error = "unknown error"
        for attempt in range(1, FETCH_RETRIES + 1):
            try:
                resp = self.session.get(url, params=params, timeout=REQUEST_TIMEOUT)
            except requests.RequestException as exc:
                last_error = f"network error: {exc}"
            else:
                if resp.status_code == 200:
                    return resp
                if resp.status_code == 429:
                    retry_after = max(float(resp.headers.get("Retry-After") or FETCH_RETRY_DELAY), 1.0)
                    last_error = "rate limited (429)"
                    print(f"\n  Rate limited; retrying in {retry_after:.0f}s...")
                    time.sleep(retry_after)
                    continue
                last_error = f"HTTP {resp.status_code}"
                if resp.status_code in (401, 403, 404):
                    break  # authentication or access errors will not succeed on retry
            if attempt < FETCH_RETRIES:
                print(f"\n  {last_error}; retrying in {FETCH_RETRY_DELAY}s "
                      f"(attempt {attempt}/{FETCH_RETRIES})...")
                time.sleep(FETCH_RETRY_DELAY)
        raise HistoryFetchError(last_error)
    
    def delete_message(self, channel_id, message_id):
        """Delete a single message. Returns the response so the caller can
        read status_code (and Retry-After when rate limited)."""
        return self.session.delete(
            f"{API_BASE}/channels/{channel_id}/messages/{message_id}",
            timeout=REQUEST_TIMEOUT)

    def download_attachment(self, url, dest_path):
        """Stream an attachment to dest_path via a .part file, renamed on completion.
        Returns bytes written, or None on failure."""
        tmp_path = dest_path.with_name(dest_path.name + ".part")
        try:
            resp = requests.get(url, stream=True, timeout=30)
            if resp.status_code != 200:
                return None
            size = 0
            with open(tmp_path, "wb") as f:
                for chunk in resp.iter_content(chunk_size=8192):
                    f.write(chunk)
                    size += len(chunk)
            tmp_path.replace(dest_path)
            return size
        except (requests.RequestException, OSError):
            try:
                tmp_path.unlink()
            except OSError:
                pass
            return None
    
    def delete_message_with_retries(self, channel_id, message_id):
        """Delete one message, retrying when rate limited.

        Returns (status, wait) where status is one of:
          "deleted"      — the message is gone; wait is the extra seconds the
                           server asked for before the next delete when the
                           rate bucket is exhausted (0 otherwise)
          "skipped"      — a system message (call, pin notification) the API
                           can never delete; the rest of the run is unaffected
          "failed"       — an unexpected error on this message; the run continues
          "auth"         — 401/403: the token was rejected or this account
                           cannot delete in this channel; every further delete
                           would fail the same way
          "rate_limited" — still 429 after DELETE_RETRIES attempts; the server
                           is still throttling, so every remaining delete
                           would burn the same backoff
        A 429 backs off for the server's Retry-After and retries the same
        message (bounded)."""
        for attempt in range(1, DELETE_RETRIES + 1):
            resp = self.delete_message(channel_id, message_id)
            if resp.status_code in (200, 204):
                wait = 0.0
                if (resp.headers.get("X-RateLimit-Remaining") or "1") == "0":
                    try:
                        wait = float(resp.headers.get("X-RateLimit-Reset-After") or 0)
                    except ValueError:
                        wait = 0.0
                return "deleted", wait
            if resp.status_code == 429:
                retry_after = max(float(resp.headers.get("Retry-After") or RATE_LIMIT_DELAY), 1.0)
                print(f"\n  Rate limited; retrying in {retry_after:.0f}s "
                      f"(attempt {attempt}/{DELETE_RETRIES})...")
                time.sleep(retry_after)
                continue
            if resp.status_code in (401, 403):
                if resp.status_code == 403 and _error_code(resp) in UNDELETABLE_MESSAGE_CODES:
                    # This one message can never be deleted (a system
                    # message such as a call or pin notification); the
                    # rest of the run is unaffected.
                    return "skipped", 0.0
                reason = ("the token was rejected - re-authenticate"
                          if resp.status_code == 401 else
                          "this account is not allowed to delete messages "
                          "in this channel")
                print(f"\n\nStopping: delete returned HTTP {resp.status_code} ({reason}).")
                detail = _error_detail(resp)
                if detail:
                    print(f"Platform response: {detail}")
                return "auth", 0.0
            return "failed", 0.0
        print(f"\n\nStopping: still rate limited after {DELETE_RETRIES} attempts. "
              f"Every remaining delete would fail the same way.")
        return "rate_limited", 0.0
