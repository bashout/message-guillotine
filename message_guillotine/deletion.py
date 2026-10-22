"""Streaming deletion runs with resumable per-channel state.

A run scans the channel newest-to-oldest one fetched page at a time and
deletes each of your messages in the chosen range as it is found, so
progress shows from the first page and the channel is never buffered in
memory. Progress is persisted to a small per-channel session file (a
`before` cursor plus counts); an interrupted run — abort, crash, Ctrl-C —
resumes where it stopped without rescanning or re-confirming, and the
session file is removed once the run completes.
"""


import json
import threading
import time
from datetime import datetime, timedelta
from pathlib import Path

from message_guillotine.api import (
    RATE_LIMIT_DELAY,
    SYSTEM_MESSAGE_TYPES,
    HistoryFetchError,
    snowflake_for,
)
from message_guillotine.config import app_config_dir

SAMPLE_MAX = 3  # messages quoted in a dry-run summary


def _start_total_count(tool, channel_id, me_id, min_id, max_id):
    """Kick off tool.count_my_messages in the background so the run starts
    scanning immediately instead of waiting on the count. Returns
    poll() -> total-or-None; a tool without the method (stubs, older
    callers) polls None forever and the run just shows no estimate."""
    if not hasattr(tool, "count_my_messages"):
        return lambda: None
    box = {}

    def worker():
        try:
            box["total"] = tool.count_my_messages(channel_id, me_id, min_id, max_id)
        except Exception:
            box["total"] = None

    threading.Thread(target=worker, daemon=True).start()
    return lambda: box.get("total")


def sessions_root():
    """Directory holding one deletion session file per channel."""
    return app_config_dir() / "deletion_sessions"


def session_path(channel_id, root=None):
    """The session file for this channel under root (default: the app's
    deletion sessions directory)."""
    return (Path(root) if root is not None else sessions_root()) / f"{channel_id}.json"


def load_session(channel_id, root=None):
    """The unfinished deletion session for this channel, or None. A session
    marked complete (or a corrupt file) is treated as absent."""
    try:
        state = json.loads(session_path(channel_id, root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(state, dict) or state.get("complete"):
        return None
    return state


def save_session(channel_id, state, root=None):
    """Persist the run state atomically (temp file + rename). Resume is a
    convenience: if the write fails the deletes already stand, the run just
    rescans further next time."""
    path = session_path(channel_id, root)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = path.with_name(path.name + ".part")
        tmp_path.write_text(json.dumps(state) + "\n", encoding="utf-8")
        tmp_path.replace(path)
    except OSError:
        pass


def clear_session(channel_id, root=None):
    """Remove the session file: the run completed, nothing left to resume."""
    try:
        session_path(channel_id, root).unlink()
    except OSError:
        pass


def _epoch(timestamp):
    """A message timestamp (ISO, UTC) as seconds since the epoch."""
    return datetime.fromisoformat(timestamp.replace("Z", "+00:00")).timestamp()


def _fmt_elapsed(seconds):
    seconds = int(seconds)
    hours, seconds = divmod(seconds, 3600)
    minutes, seconds = divmod(seconds, 60)
    if hours:
        return f"{hours}:{minutes:02d}:{seconds:02d}"
    return f"{minutes:02d}:{seconds:02d}"


def _sample(msg):
    """A one-line quote of a message, for dry-run summaries."""
    author = msg.get("author") or {}
    name = author.get("global_name") or author.get("username") or author.get("id")
    content = " ".join((msg.get("content") or "").split())
    if len(content) > 60:
        content = content[:57] + "..."
    return f"{name}: {content}"


def run_deletion(tool, channel_id, label, start_time=None, end_time=None,
                 resume=None, dry_run=False, root=None):
    """Stream-scan the channel newest-to-oldest and delete every message you
    sent inside [start_time, end_time] (either bound may be None = no limit).
    With an end_time the scan starts at the bound's snowflake — message IDs
    encode creation time, so nothing newer than the range is ever fetched.

    With resume (a session dict from load_session), the scan continues after
    its before-cursor and the counters carry over. With dry_run, messages
    are only counted and quoted (up to SAMPLE_MAX samples), nothing is
    deleted and no session is written.

    Returns a summary dict: deleted, failed, skipped, scanned, complete
    (True when the scan ran to the range boundary), aborted (a short reason
    string when the run stopped early, else None), samples.

    In parallel with the scan, the platform's search endpoint is asked how
    many of your messages lie in the remaining range; while that count is
    known, the progress line carries a remaining count and, for real runs,
    an ETA at the delete pace. Search being unavailable just drops the
    estimate — the run is unaffected.

    The per-channel session file is updated after every scanned message, so
    any interruption leaves a resumable run behind; a completed run removes
    it.
    """
    channel_id = str(channel_id)
    start_ts = start_time.timestamp() if start_time is not None else None
    end_ts = end_time.timestamp() if end_time is not None else None

    state = {
        "channel_id": channel_id,
        "label": label,
        "start_time": start_time.isoformat() if start_time is not None else None,
        "end_time": end_time.isoformat() if end_time is not None else None,
        "before_cursor": None,
        "scanned": 0,
        "deleted": 0,
        "failed": 0,
        "skipped": 0,
    }
    if resume:
        for key in ("scanned", "deleted", "failed", "skipped"):
            state[key] = int(resume.get(key) or 0)
        state["before_cursor"] = resume.get("before_cursor")
    elif end_time is not None:
        # Jump straight to the range boundary instead of scanning from the
        # channel top and skipping every newer message: `before` the end
        # bound's snowflake starts the first page inside the range. +1ms
        # keeps the bound inclusive (`before` is strict); the timestamp
        # check in the scan stays as the safety net.
        state["before_cursor"] = snowflake_for(
            end_time + timedelta(milliseconds=1))

    # Counters at the moment the scan starts. The total below covers only
    # the range ahead of the cursor, so on a resume the prior session's
    # deletes — all behind the cursor — must not be subtracted again.
    prior_deleted = state["deleted"]
    prior_failed = state["failed"]

    me_id = (tool.current_user or {}).get("id")
    # The count query covers exactly the range still ahead of the cursor: a
    # resume counts only messages older than where it stopped.
    poll_total = _start_total_count(
        tool, channel_id, me_id,
        snowflake_for(start_time) if start_time is not None else None,
        (state["before_cursor"] if state["before_cursor"] is not None
         else snowflake_for(end_time) if end_time is not None else None))
    samples = []
    aborted = None
    aborted_message = None
    pending_pace = 0.0  # seconds owed before the next delete, from the last one
    start_clock = time.time()

    def estimate_segment(dry=dry_run):
        """Progress-line tail with the remaining count (and ETA for real
        runs), or "" while the search count is unavailable. The count comes
        from the search index and covers exactly the range ahead of the
        cursor, so only what this run has deleted or failed to delete is
        subtracted as it ticks; a resume's prior counters are all behind
        the cursor and stay out of the arithmetic. The ETA (delete pace)
        is an estimate, not a promise."""
        total = poll_total()
        if total is None:
            return ""
        remaining = max(0, total - (state["deleted"] - prior_deleted)
                         - (state["failed"] - prior_failed))
        if dry:
            return f" | ~{remaining} of yours left"
        return (f" | ~{remaining} left"
                f" | ETA {_fmt_elapsed(remaining * RATE_LIMIT_DELAY)}")

    def progress(dry=dry_run):
        elapsed = _fmt_elapsed(time.time() - start_clock)
        if dry:
            print(f"\r[scanned {state['scanned']} | would delete {state['deleted']}"
                  f"{estimate_segment()} | {elapsed}]", end="", flush=True)
        else:
            print(f"\r[scanned {state['scanned']} | deleted {state['deleted']}"
                  f" | failed {state['failed']} | skipped {state['skipped']}"
                  f"{estimate_segment()} | {elapsed}]", end="", flush=True)

    try:
        for msg in tool.iter_channel_history_before(
                channel_id, before_id=state["before_cursor"]):
            ts = _epoch(msg["timestamp"])
            if end_ts is not None and ts > end_ts:
                continue  # newer than the range: keep scanning backwards
            if start_ts is not None and ts < start_ts:
                break  # older than the range: nothing further can match
            state["scanned"] += 1
            state["before_cursor"] = msg["id"]

            if (msg.get("author") or {}).get("id") != me_id:
                if not dry_run:
                    save_session(channel_id, state, root)
                continue

            if msg.get("type", 0) in SYSTEM_MESSAGE_TYPES:
                # Calls, pin notifications and friends can never be deleted;
                # skipping them locally costs no request, so a chat that
                # starts with a stack of them reaches the first real message
                # at scan speed instead of grinding through 429 backoffs.
                state["skipped"] += 1
                progress()
                if not dry_run:
                    save_session(channel_id, state, root)
                continue

            if dry_run:
                state["deleted"] += 1
                if len(samples) < SAMPLE_MAX:
                    samples.append(_sample(msg))
                progress()
                continue

            status, wait = tool.delete_message_with_retries(channel_id, msg["id"])
            if status == "deleted":
                if pending_pace:
                    time.sleep(pending_pace)  # pace between deletes only
                pending_pace = max(RATE_LIMIT_DELAY, wait)
                state["deleted"] += 1
            elif status == "skipped":
                state["skipped"] += 1
            elif status == "failed":
                state["failed"] += 1
            else:  # "auth", "rate_limited" or "network": every further
                # delete fails the same way; the session file lets a
                # network/rate-limit abort resume where it stopped
                aborted = status
                break
            progress()
            save_session(channel_id, state, root)
    except KeyboardInterrupt:
        aborted = "interrupted"
        aborted_message = "\nInterrupted."
    except HistoryFetchError as exc:
        aborted = "fetch failed"
        aborted_message = f"\nFetch failed ({exc})."

    print()  # finish the progress line
    if aborted_message:
        print(aborted_message)
    if aborted is None:
        clear_session(channel_id, root)
    else:
        save_session(channel_id, state, root)

    state["complete"] = aborted is None
    state["aborted"] = aborted
    state["samples"] = samples
    return state
