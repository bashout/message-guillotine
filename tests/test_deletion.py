"""Message deletion: single-message retries, streaming runs, resume state."""

import time
from datetime import datetime

from message_guillotine.api import RATE_LIMIT_DELAY, HistoryFetchError, MessageGuillotine
from message_guillotine.deletion import (
    clear_session,
    load_session,
    run_deletion,
    save_session,
)


# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------

class FakeResponse:
    def __init__(self, status_code, headers=None, body=None):
        self.status_code = status_code
        self.headers = headers or {}
        self._body = body

    def json(self):
        return self._body


class FakeSession:
    """Canned delete responses in order; records every call."""

    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = []

    def delete(self, url, timeout=None):
        self.calls.append(url)
        item = self.responses.pop(0)
        return item if isinstance(item, FakeResponse) else FakeResponse(item)


class StubTool:
    """Minimal tool for runner tests: a fixed message stream plus canned
    delete results."""

    def __init__(self, messages, delete_results=None, me="7", exc=None):
        self.current_user = {"id": me}
        self.messages = list(messages)
        self.delete_results = list(delete_results or [])
        self.delete_calls = []
        self.fetch_calls = []
        self.exc = exc  # raised once the stream is exhausted

    def iter_channel_history_before(self, channel_id, before_id=None, limit=100):
        self.fetch_calls.append(before_id)
        for msg in self.messages:
            yield msg
        if self.exc is not None:
            raise self.exc

    def delete_message_with_retries(self, channel_id, message_id):
        self.delete_calls.append(message_id)
        result = self.delete_results.pop(0)
        return result if isinstance(result, tuple) else (result, 0.0)


def make_tool(responses):
    tool = MessageGuillotine.__new__(MessageGuillotine)
    tool.current_user = {"id": "7"}
    tool.session = FakeSession(responses)
    return tool


def message(i, author_id="7", day=5, content=None, msg_type=0):
    return {"id": str(i), "author": {"id": author_id},
            "timestamp": f"2026-01-{day:02d}T00:00:00+00:00",
            "content": content or f"msg {i}",
            "type": msg_type}


def patch_sleep(monkeypatch):
    sleeps = []
    monkeypatch.setattr(time, "sleep", lambda s: sleeps.append(s))
    return sleeps


# ---------------------------------------------------------------------------
# delete_message_with_retries: statuses, 429 backoff, auth aborts
# ---------------------------------------------------------------------------

def test_delete_ok_returns_deleted(monkeypatch):
    tool = make_tool([204])
    assert tool.delete_message_with_retries("42", "1") == ("deleted", 0.0)


def test_delete_reports_wait_when_bucket_exhausted():
    tool = make_tool([FakeResponse(204, {"X-RateLimit-Remaining": "0",
                                         "X-RateLimit-Reset-After": "12"})])
    assert tool.delete_message_with_retries("42", "1") == ("deleted", 12.0)


def test_delete_429_retries_after_retry_after(monkeypatch):
    tool = make_tool([FakeResponse(429, {"Retry-After": "7"}), 204])
    sleeps = patch_sleep(monkeypatch)
    status, wait = tool.delete_message_with_retries("42", "1")
    assert (status, wait) == ("deleted", 0.0)
    assert len(tool.session.calls) == 2  # the same message retried
    assert sleeps == [7.0]


def test_delete_403_system_message_skips():
    class SystemMessageResponse(FakeResponse):
        def __init__(self):
            super().__init__(403)
            self._body = {"message": "Cannot execute action on a system message",
                          "code": 50021}

    tool = make_tool([SystemMessageResponse()])
    assert tool.delete_message_with_retries("42", "1") == ("skipped", 0.0)


def test_delete_403_other_codes_abort(monkeypatch, capsys):
    class PermissionResponse(FakeResponse):
        def __init__(self):
            super().__init__(403)
            self._body = {"message": "Missing Permissions", "code": 50013}

    tool = make_tool([PermissionResponse()])
    assert tool.delete_message_with_retries("42", "1") == ("auth", 0.0)
    out = capsys.readouterr().out
    assert "HTTP 403" in out
    assert "Missing Permissions (code 50013)" in out
    assert "Stopping" in out


def test_delete_401_aborts(monkeypatch, capsys):
    tool = make_tool([401])
    assert tool.delete_message_with_retries("42", "1") == ("auth", 0.0)
    out = capsys.readouterr().out
    assert "HTTP 401" in out
    assert "token was rejected" in out


def test_delete_unexpected_status_fails_message():
    tool = make_tool([500])
    assert tool.delete_message_with_retries("42", "1") == ("failed", 0.0)


def test_delete_exhausted_429_aborts(monkeypatch, capsys):
    tool = make_tool([FakeResponse(429, {"Retry-After": "2"})] * 3)
    sleeps = patch_sleep(monkeypatch)
    status, _ = tool.delete_message_with_retries("42", "1")
    assert status == "rate_limited"
    assert len(tool.session.calls) == 3  # three attempts on the same message
    assert sleeps == [2.0, 2.0, 2.0]
    assert "still rate limited" in capsys.readouterr().out


# ---------------------------------------------------------------------------
# session state
# ---------------------------------------------------------------------------

def test_session_save_load_clear_roundtrip(tmp_path):
    state = {"channel_id": "42", "deleted": 3, "before_cursor": "99",
             "complete": False}
    save_session("42", state, root=tmp_path)
    loaded = load_session("42", root=tmp_path)
    assert loaded["deleted"] == 3 and loaded["before_cursor"] == "99"
    clear_session("42", root=tmp_path)
    assert load_session("42", root=tmp_path) is None


def test_load_session_treats_complete_as_absent(tmp_path):
    save_session("42", {"channel_id": "42", "complete": True}, root=tmp_path)
    assert load_session("42", root=tmp_path) is None


def test_load_session_missing_file_is_none(tmp_path):
    assert load_session("42", root=tmp_path) is None


# ---------------------------------------------------------------------------
# run_deletion: streaming scan, range bounds, resume, dry run
# ---------------------------------------------------------------------------

def test_run_deletes_only_own_messages_and_paces(monkeypatch, tmp_path):
    tool = StubTool([message(5), message(4, author_id="999"), message(3)],
                    delete_results=[("deleted", 0.0), ("deleted", 0.0)])
    sleeps = patch_sleep(monkeypatch)
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert tool.delete_calls == ["5", "3"]  # only mine, newest first
    assert sleeps == [RATE_LIMIT_DELAY]  # pace between deletes, none trailing
    assert summary["deleted"] == 2 and summary["scanned"] == 3
    assert summary["complete"] is True
    assert load_session("42", root=tmp_path) is None  # completed: no session


def test_run_stops_scanning_at_start_boundary(monkeypatch, tmp_path):
    tool = StubTool([message(5), message(4, day=4), message(3, day=3)],
                    delete_results=[("deleted", 0.0), ("deleted", 0.0)])
    run_deletion(tool, "42", "chat",
                 start_time=datetime(2026, 1, 4), root=tmp_path)
    assert tool.delete_calls == ["5", "4"]  # day 3 is older than the range
    assert load_session("42", root=tmp_path) is None


def test_run_skips_messages_newer_than_end_time(monkeypatch, tmp_path):
    tool = StubTool([message(5), message(4, day=4), message(3, day=3)],
                    delete_results=[("deleted", 0.0), ("deleted", 0.0)])
    summary = run_deletion(tool, "42", "chat",
                           end_time=datetime(2026, 1, 4), root=tmp_path)
    assert tool.delete_calls == ["4", "3"]  # the newer message is out of range
    assert summary["scanned"] == 2


def test_run_resumes_from_saved_cursor_and_counts(monkeypatch, tmp_path):
    tool = StubTool([message(3, day=3), message(2, day=2)],
                    delete_results=[("deleted", 0.0), ("deleted", 0.0)])
    resume = {"channel_id": "42", "before_cursor": "4",
              "scanned": 10, "deleted": 4, "failed": 0, "skipped": 1}
    summary = run_deletion(tool, "42", "chat", resume=resume, root=tmp_path)
    assert tool.fetch_calls == ["4"]  # continues after the cursor, no rescan
    assert summary["deleted"] == 6 and summary["scanned"] == 12
    assert summary["skipped"] == 1
    assert summary["complete"] is True


def test_run_auth_abort_saves_resumable_session(monkeypatch, tmp_path):
    tool = StubTool([message(5)], delete_results=[("auth", 0.0)])
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert summary["complete"] is False
    assert summary["aborted"] == "auth"
    session = load_session("42", root=tmp_path)
    assert session["before_cursor"] == "5" and session["scanned"] == 1


def test_run_interrupt_saves_resumable_session(monkeypatch, tmp_path, capsys):
    tool = StubTool([], exc=KeyboardInterrupt)
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert summary["aborted"] == "interrupted"
    assert load_session("42", root=tmp_path) is not None
    assert "Interrupted" in capsys.readouterr().out


def test_run_fetch_failure_saves_resumable_session(monkeypatch, tmp_path, capsys):
    tool = StubTool([], exc=HistoryFetchError("HTTP 500"))
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert summary["aborted"] == "fetch failed"
    assert "Fetch failed (HTTP 500)" in capsys.readouterr().out
    assert load_session("42", root=tmp_path) is not None


def test_run_failed_message_is_counted_and_run_continues(monkeypatch, tmp_path):
    tool = StubTool([message(5), message(4, day=4)],
                    delete_results=[("failed", 0.0), ("deleted", 0.0)])
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert summary["failed"] == 1 and summary["deleted"] == 1
    assert summary["complete"] is True


def test_dry_run_deletes_nothing_and_writes_no_session(monkeypatch, tmp_path):
    tool = StubTool([message(5), message(4, author_id="999"),
                     message(3, day=3, content="hello world")])
    summary = run_deletion(tool, "42", "chat", dry_run=True, root=tmp_path)
    assert tool.delete_calls == []
    assert summary["deleted"] == 2  # counted as "would delete"
    assert summary["samples"] == ["7: msg 5", "7: hello world"]
    assert load_session("42", root=tmp_path) is None


def test_run_skipped_system_messages_counted_inline(monkeypatch, tmp_path):
    tool = StubTool([message(5), message(4, day=4)],
                    delete_results=[("skipped", 0.0), ("deleted", 0.0)])
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert summary["skipped"] == 1 and summary["deleted"] == 1


def test_run_skips_system_messages_locally_without_api_calls(monkeypatch, tmp_path):
    # A stack of call/pin notifications at the top of the chat: none may
    # cost a delete request (the delete bucket is what 429s), the first
    # real message is reached at scan speed.
    tool = StubTool([message(9, msg_type=3), message(8, msg_type=6),
                     message(7, msg_type=1), message(6, msg_type=4),
                     message(5)],
                    delete_results=[("deleted", 0.0)])
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert tool.delete_calls == ["5"]  # only the tangible message, no 429 fodder
    assert summary["skipped"] == 4
    assert summary["deleted"] == 1
    assert summary["complete"] is True


def test_run_others_system_messages_never_reach_delete(monkeypatch, tmp_path):
    # System messages authored by the other party are dropped by the
    # mine-only check before the system-type skip even applies.
    tool = StubTool([message(9, author_id="999", msg_type=3), message(5)],
                    delete_results=[("deleted", 0.0)])
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert tool.delete_calls == ["5"]
    assert summary["scanned"] == 2
    assert summary["skipped"] == 0


def test_dry_run_counts_system_messages_as_skipped_not_deletable(monkeypatch, tmp_path):
    tool = StubTool([message(9, msg_type=3), message(5, content="real message")])
    summary = run_deletion(tool, "42", "chat", dry_run=True, root=tmp_path)
    assert summary["deleted"] == 1  # the system message is not "would delete"
    assert summary["skipped"] == 1
    assert summary["samples"] == ["7: real message"]


# ---------------------------------------------------------------------------
# session headers (unchanged behavior, kept from the old delete path)
# ---------------------------------------------------------------------------

def test_session_headers_mimic_web_client():
    import base64 as b64
    import json as json_mod

    tool = MessageGuillotine("fake-token")
    headers = tool.session.headers
    assert headers["Authorization"] == "fake-token"
    # A complete, real browser UA — not a truncated pseudo-browser string
    assert "Chrome/" in headers["User-Agent"] and "Safari/537.36" in headers["User-Agent"]
    props = json_mod.loads(b64.b64decode(headers["X-Super-Properties"]))
    assert props["os"] == "Windows" and props["browser"] == "Chrome"
    assert props["browser_user_agent"] == headers["User-Agent"]
    assert headers["X-Discord-Locale"] == "en-US"
