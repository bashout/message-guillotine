"""Message deletion: single-message retries, streaming runs, resume state."""

import threading
import time
from datetime import datetime, timedelta, timezone

import requests

from message_guillotine import deletion
from message_guillotine.api import (
    DISCORD_EPOCH_MS,
    FETCH_RETRY_DELAY,
    RATE_LIMIT_DELAY,
    HistoryFetchError,
    MessageGuillotine,
    snowflake_for,
)
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
        if isinstance(item, Exception):
            raise item
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


def test_delete_network_error_retries_and_deletes(monkeypatch):
    # The server closes the pooled keep-alive connection while the run
    # paces between deletes; the retry lands on a fresh connection.
    tool = make_tool([requests.ConnectionError("Remote end closed connection"
                                               " without response"), 204])
    sleeps = patch_sleep(monkeypatch)
    status, wait = tool.delete_message_with_retries("42", "1")
    assert (status, wait) == ("deleted", 0.0)
    assert len(tool.session.calls) == 2  # the same message retried
    assert sleeps == [FETCH_RETRY_DELAY]


def test_delete_404_after_network_error_counts_as_deleted(monkeypatch):
    # The first attempt's delete reached the platform; only its reply was
    # lost. The retry's 404 means the message is already gone.
    tool = make_tool([requests.ConnectionError("Remote end closed"), 404])
    status, _ = tool.delete_message_with_retries("42", "1")
    assert status == "deleted"


def test_delete_404_without_network_error_stays_failed():
    tool = make_tool([404])
    assert tool.delete_message_with_retries("42", "1") == ("failed", 0.0)


def test_delete_persistent_network_errors_abort_as_network(monkeypatch, capsys):
    tool = make_tool([requests.ConnectionError("down")] * 3)
    sleeps = patch_sleep(monkeypatch)
    status, _ = tool.delete_message_with_retries("42", "1")
    assert status == "network"
    assert len(tool.session.calls) == 3
    assert sleeps == [FETCH_RETRY_DELAY] * 3
    assert "network errors persisted" in capsys.readouterr().out


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


def test_run_with_end_time_jumps_scan_to_range_boundary(monkeypatch, tmp_path):
    # A fresh run with an end bound seeds the cursor with the bound's
    # snowflake, so the first fetch is already inside the range: months of
    # newer history are never fetched, and a message created at exactly the
    # bound still counts as inside (`before` is strict, hence the +1ms).
    def snowflake_at(dt):
        return str((int(dt.timestamp() * 1000) - DISCORD_EPOCH_MS) << 22)

    end = datetime(2026, 1, 5, tzinfo=timezone.utc)
    msgs = [
        {"id": snowflake_at(datetime(2026, 1, 6, tzinfo=timezone.utc)),
         "author": {"id": "7"}, "timestamp": "2026-01-06T00:00:00+00:00",
         "content": "newer", "type": 0},
        {"id": snowflake_at(end),
         "author": {"id": "7"}, "timestamp": "2026-01-05T00:00:00+00:00",
         "content": "at the bound", "type": 0},
        {"id": snowflake_at(datetime(2026, 1, 3, tzinfo=timezone.utc)),
         "author": {"id": "7"}, "timestamp": "2026-01-03T00:00:00+00:00",
         "content": "older", "type": 0},
    ]

    class BoundaryTool(StubTool):
        def iter_channel_history_before(self, channel_id, before_id=None, limit=100):
            self.fetch_calls.append(before_id)
            for msg in self.messages:
                if before_id is not None and int(msg["id"]) >= int(before_id):
                    continue
                yield msg

    tool = BoundaryTool(msgs, delete_results=[("deleted", 0.0)])
    summary = run_deletion(tool, "42", "chat",
                           start_time=datetime(2026, 1, 4, tzinfo=timezone.utc),
                           end_time=end, root=tmp_path)
    assert tool.fetch_calls == [snowflake_for(end + timedelta(milliseconds=1))]
    assert tool.delete_calls == [snowflake_at(end)]
    assert summary["deleted"] == 1 and summary["complete"] is True


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


def test_run_network_abort_saves_resumable_session(monkeypatch, tmp_path):
    tool = StubTool([message(5)], delete_results=[("network", 0.0)])
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    assert summary["complete"] is False
    assert summary["aborted"] == "network"
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
# remaining count / ETA (search-endpoint count fetched in the background)
# ---------------------------------------------------------------------------

def test_start_total_count_polls_thread_result():
    class CountTool:
        def __init__(self):
            self.calls = []

        def count_my_messages(self, channel_id, author_id, min_id, max_id):
            self.calls.append((channel_id, author_id, min_id, max_id))
            return 42

    tool = CountTool()
    poll = deletion._start_total_count(tool, "42", "7", "100", "900")
    deadline = time.time() + 5
    got = None
    while got is None and time.time() < deadline:
        got = poll()
        if got is None:
            threading.Event().wait(0.01)
    assert got == 42
    assert tool.calls == [("42", "7", "100", "900")]


def test_start_total_count_without_method_polls_none():
    poll = deletion._start_total_count(StubTool([]), "42", "7", None, None)
    assert poll() is None


def test_run_shows_remaining_and_eta(monkeypatch, tmp_path, capsys):
    tool = StubTool([message(5), message(4, day=4), message(3, day=3)],
                    delete_results=[("deleted", 0.0)] * 3)
    monkeypatch.setattr(deletion, "_start_total_count",
                        lambda tool, channel_id, me_id, min_id, max_id:
                        (lambda: 10))
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    out = capsys.readouterr().out
    assert "~9 left" in out  # 10 - 1 after the first delete
    assert "~7 left" in out  # 10 - 3 after the last
    assert "ETA" in out
    assert summary["deleted"] == 3


def test_run_without_count_shows_no_eta(monkeypatch, tmp_path, capsys):
    # StubTool has no count_my_messages: the run must proceed with the
    # plain progress line, no estimate segments.
    tool = StubTool([message(5)], delete_results=[("deleted", 0.0)])
    summary = run_deletion(tool, "42", "chat", root=tmp_path)
    out = capsys.readouterr().out
    assert "ETA" not in out and "~" not in out
    assert summary["deleted"] == 1


def test_run_bounds_count_by_range_and_cursor(monkeypatch, tmp_path):
    seen = {}

    def fake_start(tool, channel_id, me_id, min_id, max_id):
        seen.update(min_id=min_id, max_id=max_id)
        return lambda: None

    monkeypatch.setattr(deletion, "_start_total_count", fake_start)
    start_time = datetime(2026, 1, 4)
    end_time = datetime(2026, 1, 6)
    tool = StubTool([message(5)], delete_results=[("deleted", 0.0)])
    run_deletion(tool, "42", "chat", start_time=start_time,
                 end_time=end_time, root=tmp_path)
    assert seen["min_id"] == snowflake_for(start_time)
    # The count is bounded by the scan cursor, which a fresh run seeds at
    # the end bound (+1ms: `before` is strict), so the estimate covers
    # exactly the range ahead of the scan.
    assert seen["max_id"] == snowflake_for(end_time + timedelta(milliseconds=1))


def test_run_resume_counts_only_messages_after_cursor(monkeypatch, tmp_path):
    seen = {}

    def fake_start(tool, channel_id, me_id, min_id, max_id):
        seen.update(me_id=me_id, min_id=min_id, max_id=max_id)
        return lambda: None

    monkeypatch.setattr(deletion, "_start_total_count", fake_start)
    tool = StubTool([message(3, day=3)], delete_results=[("deleted", 0.0)])
    resume = {"channel_id": "42", "before_cursor": "4",
              "scanned": 10, "deleted": 4, "failed": 0, "skipped": 1,
              "start_time": None, "end_time": None}
    run_deletion(tool, "42", "chat", resume=resume, root=tmp_path)
    assert seen == {"me_id": "7", "min_id": None, "max_id": "4"}


def test_run_resume_estimate_ignores_prior_counters(monkeypatch, tmp_path, capsys):
    # The resume count covers only messages ahead of the cursor; the prior
    # session's deleted/failed are behind it, so subtracting them again
    # would undercount what is left (and with it the ETA).
    tool = StubTool([message(3, day=3), message(2, day=2)],
                    delete_results=[("deleted", 0.0), ("deleted", 0.0)])
    patch_sleep(monkeypatch)
    monkeypatch.setattr(deletion, "_start_total_count",
                        lambda *args: (lambda: 2))
    resume = {"channel_id": "42", "before_cursor": "4",
              "scanned": 10, "deleted": 4, "failed": 1, "skipped": 1,
              "start_time": None, "end_time": None}
    run_deletion(tool, "42", "chat", resume=resume, root=tmp_path)
    out = capsys.readouterr().out
    assert "~1 left" in out  # 2 ahead - 1 deleted this run
    assert "~0 left" in out  # 2 ahead - 2 deleted this run


def test_dry_run_shows_remaining_without_eta(monkeypatch, tmp_path, capsys):
    tool = StubTool([message(5), message(3, day=3)])
    monkeypatch.setattr(deletion, "_start_total_count",
                        lambda *args: (lambda: 5))
    summary = run_deletion(tool, "42", "chat", dry_run=True, root=tmp_path)
    out = capsys.readouterr().out
    assert "~3 of yours left" in out  # 5 - 2 would-deletes
    assert "ETA" not in out
    assert summary["deleted"] == 2
