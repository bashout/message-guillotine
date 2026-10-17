"""Interactive CLI helpers."""

import time
from datetime import datetime, timedelta, timezone

from message_guillotine import cli
from message_guillotine.deletion import save_session


def test_parse_datetime_blank_is_none(monkeypatch):
    monkeypatch.setattr("builtins.input", lambda _: "")
    assert cli.parse_datetime("date: ") is None


def test_parse_datetime_accepted_formats(monkeypatch):
    answers = iter(["2026-01-02", "2026-01-02 03:04", "01/03/2026",
                    "2026-01-02T05:06:07+00:00"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert cli.parse_datetime("d") == datetime(2026, 1, 2)
    assert cli.parse_datetime("d") == datetime(2026, 1, 2, 3, 4)
    assert cli.parse_datetime("d") == datetime(2026, 1, 3)
    assert cli.parse_datetime("d") == datetime(2026, 1, 2, 5, 6, 7,
                                               tzinfo=timezone.utc)


def test_parse_datetime_relative_forms(monkeypatch):
    today = datetime.now().replace(hour=0, minute=0, second=0, microsecond=0)
    answers = iter(["today", "yesterday", "7d", "2w", "3d ago"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert cli.parse_datetime("d") == today
    assert cli.parse_datetime("d") == today - timedelta(days=1)
    assert cli.parse_datetime("d") == today - timedelta(days=7)
    assert cli.parse_datetime("d") == today - timedelta(days=14)
    assert cli.parse_datetime("d") == today - timedelta(days=3)


def test_parse_datetime_retries_on_invalid_input(monkeypatch, capsys):
    answers = iter(["nonsense", "2026-01-02"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert cli.parse_datetime("d") == datetime(2026, 1, 2)
    assert "Invalid format" in capsys.readouterr().out


def _dm_user(i, name=None):
    return {"id": str(100 + i), "username": name or f"user{i}",
            "discriminator": "0001", "global_name": "",
            "channel_id": str(i), "last_message_id": str(1000 - i)}


def test_select_dm_user_returns_choice(monkeypatch):
    class StubTool:
        def get_dm_channels(self):
            return [{"id": "1", "recipients": []}]

        def get_all_dm_users(self):
            return [_dm_user(0, name="bob")]

    monkeypatch.setattr("builtins.input", lambda _: "1")
    user = cli.select_dm_user(StubTool())
    assert user["username"] == "bob"


def test_select_dm_user_cancel(monkeypatch):
    class StubTool:
        def get_dm_channels(self):
            return [{"id": "1", "recipients": []}]

        def get_all_dm_users(self):
            return [_dm_user(0, name="bob")]

    monkeypatch.setattr("builtins.input", lambda _: "0")
    assert cli.select_dm_user(StubTool()) is None


def test_select_dm_user_invalid_pick_reprompts(monkeypatch, capsys):
    class StubTool:
        def get_dm_channels(self):
            return [{"id": "1", "recipients": []}]

        def get_all_dm_users(self):
            return [_dm_user(0, name="bob")]

    answers = iter(["1", "x", "9", "1"])  # sort, two bad picks, then valid
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    user = cli.select_dm_user(StubTool())
    assert user["username"] == "bob"
    assert capsys.readouterr().out.count("Invalid selection.") == 2


def test_select_dm_user_filters_long_lists(monkeypatch):
    class StubTool:
        def get_dm_channels(self):
            return [{"id": "1", "recipients": []}]

        def get_all_dm_users(self):
            return [_dm_user(i, name=f"user{i}") for i in range(15)] + \
                   [_dm_user(99, name="zed")]

    answers = iter(["1", "zed", "1"])  # sort, filter, pick
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    user = cli.select_dm_user(StubTool())
    assert user["username"] == "zed"


def test_select_dm_user_filter_without_match_reprompts(monkeypatch, capsys):
    class StubTool:
        def get_dm_channels(self):
            return [{"id": "1", "recipients": []}]

        def get_all_dm_users(self):
            return [_dm_user(i, name=f"user{i}") for i in range(15)]

    answers = iter(["1", "nope", "", "1"])  # sort, no match, list all, pick
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    user = cli.select_dm_user(StubTool())
    assert user["username"] == "user0"
    out = capsys.readouterr().out
    assert "Nothing matches that filter" in out


def test_main_without_token_exits(monkeypatch, capsys):
    monkeypatch.setattr("builtins.input", lambda _: "")
    cli.main([])  # must return, not raise
    assert "No token provided" in capsys.readouterr().out


def test_main_accepts_archive_dir_arg(monkeypatch, tmp_path, capsys):
    # The --archive-dir CLI arg reaches resolve_archive_dir as its explicit arg.
    seen = {}

    def fake_resolve(explicit=None):
        seen.setdefault("calls", []).append(explicit)
        return tmp_path / "resolved"

    class StubTool:
        def __init__(self, token):
            pass

        def get_current_user(self):
            return True

    monkeypatch.setattr(cli, "resolve_archive_dir", fake_resolve)
    monkeypatch.setattr(cli, "MessageGuillotine", StubTool)
    answers = iter(["token", "0"])  # token, then cancel at the mode menu
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    cli.main(["--archive-dir", str(tmp_path / "cli")])
    assert seen["calls"][0] == str(tmp_path / "cli")  # resolve_archive_dir converts to Path
    assert "Archival folder" in capsys.readouterr().out


def _make_archive(root, name="old", channel_id="42", chat="Old Chat"):
    folder = root / name
    folder.mkdir(parents=True)
    (folder / "meta.json").write_text(
        f'{{"channel_id": "{channel_id}", "chat": "{chat}"}}\n', encoding="utf-8")
    (folder / "messages.jsonl").write_text("", encoding="utf-8")
    return folder


def test_pick_merge_target_skipped_when_chat_already_archived(monkeypatch, tmp_path):
    _make_archive(tmp_path)
    def fail(prompt):
        raise AssertionError("no prompt expected")
    monkeypatch.setattr("builtins.input", fail)
    assert cli.pick_merge_target("42", tmp_path) is None


def test_pick_merge_target_default_starts_new_archive(monkeypatch, tmp_path):
    _make_archive(tmp_path)
    monkeypatch.setattr("builtins.input", lambda _: "1")
    assert cli.pick_merge_target("43", tmp_path) is None


def test_pick_merge_target_picks_archive(monkeypatch, tmp_path):
    folder = _make_archive(tmp_path)
    answers = iter(["2", "1"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert cli.pick_merge_target("43", tmp_path) == folder


def test_pick_merge_target_cancel_aborts_run(monkeypatch, tmp_path):
    _make_archive(tmp_path)
    monkeypatch.setattr("builtins.input", lambda _: "0")
    assert cli.pick_merge_target("43", tmp_path) is False


def test_pick_merge_target_cancel_at_archive_list_aborts_run(monkeypatch, tmp_path):
    _make_archive(tmp_path)
    answers = iter(["2", "0"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert cli.pick_merge_target("43", tmp_path) is False


def test_pick_merge_target_invalid_pick_reprompts(monkeypatch, tmp_path):
    folder = _make_archive(tmp_path)
    answers = iter(["9", "x", "2", "1"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert cli.pick_merge_target("43", tmp_path) == folder


def test_pick_merge_target_invalid_first_choice_reprompts(monkeypatch, tmp_path):
    folder = _make_archive(tmp_path)
    answers = iter(["7", "nonsense", "2", "1"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert cli.pick_merge_target("43", tmp_path) == folder


def test_pick_merge_target_no_other_archives(monkeypatch, tmp_path):
    (tmp_path / "not-an-archive").mkdir()
    def fail(prompt):
        raise AssertionError("no prompt expected")
    monkeypatch.setattr("builtins.input", fail)
    assert cli.pick_merge_target("43", tmp_path) is None


# ---------------------------------------------------------------------------
# mode menu: invalid input re-prompts, tasks return to the menu
# ---------------------------------------------------------------------------

def test_main_invalid_mode_reprompts_then_exits(monkeypatch, capsys):
    class StubTool:
        def __init__(self, token):
            pass

        def get_current_user(self):
            return True

    monkeypatch.setattr(cli, "MessageGuillotine", StubTool)
    monkeypatch.setattr(cli, "resolve_archive_dir", lambda explicit=None: None)
    answers = iter(["token", "x", "9", "0"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    cli.main([])
    out = capsys.readouterr().out
    assert out.count("Invalid selection.") == 2


def test_main_returns_to_menu_after_a_task(monkeypatch, capsys):
    class StubTool:
        def __init__(self, token):
            pass

        def get_current_user(self):
            return True

        def get_dm_channels(self):
            return False  # the DM task fails fast and falls back to the menu

    monkeypatch.setattr(cli, "MessageGuillotine", StubTool)
    monkeypatch.setattr(cli, "resolve_archive_dir", lambda explicit=None: None)
    answers = iter(["token", "1", "0"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    cli.main([])
    assert capsys.readouterr().out.count("SELECT MODE") == 2


# ---------------------------------------------------------------------------
# deletion flow: plan confirmation, streaming, resume
# ---------------------------------------------------------------------------

def _msg(i, author_id="7", day=5, content=None):
    return {"id": str(i), "author": {"id": author_id},
            "timestamp": f"2026-01-{day:02d}T00:00:00+00:00",
            "content": content or f"msg {i}"}


def _deletion_tool(monkeypatch, messages, delete_results=None):
    tool = type("Tool", (), {})()
    tool.current_user = {"id": "7"}
    tool.guilds = [{"id": "g1", "name": "G"}]
    tool.get_guilds = lambda: True
    tool.get_guild_channels = lambda guild_id: [
        {"id": "c1", "name": "general", "type": 0}]
    tool.fetch_calls = []
    tool.delete_calls = []

    def iter_history(channel_id, before_id=None, limit=100):
        tool.fetch_calls.append(before_id)
        for msg in messages:
            yield msg

    def delete(channel_id, message_id):
        tool.delete_calls.append(message_id)
        return (delete_results or [("deleted", 0.0)] * len(messages)).pop(0)

    tool.iter_channel_history_before = iter_history
    tool.delete_message_with_retries = delete
    return tool


def _run_server_deletion(monkeypatch, answers, tool, tmp_path):
    monkeypatch.setattr("message_guillotine.deletion.sessions_root",
                        lambda: tmp_path)
    monkeypatch.setattr(time, "sleep", lambda s: None)
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    cli.handle_server_deletion(tool)


def test_server_deletion_confirms_plan_then_streams(monkeypatch, tmp_path, capsys):
    tool = _deletion_tool(monkeypatch, [_msg(5), _msg(4, author_id="999"), _msg(3)])
    answers = iter(["1", "1", "", "", "y"])  # server, channel, start, end, confirm
    _run_server_deletion(monkeypatch, answers, tool, tmp_path)
    assert tool.delete_calls == ["5", "3"]  # only mine, deleted as scanned
    out = capsys.readouterr().out
    assert "DELETION PLAN" in out
    assert "every message YOU sent" in out
    assert "Done. Deleted 2" in out


def test_server_deletion_declined_confirm_deletes_nothing(monkeypatch, tmp_path):
    tool = _deletion_tool(monkeypatch, [_msg(5)])
    answers = iter(["1", "1", "", "", "n"])
    _run_server_deletion(monkeypatch, answers, tool, tmp_path)
    assert tool.delete_calls == []
    assert tool.fetch_calls == []  # nothing scanned either


def test_server_deletion_offers_resume_of_unfinished_run(monkeypatch, tmp_path, capsys):
    tool = _deletion_tool(monkeypatch, [_msg(5)])
    save_session("c1", {"channel_id": "c1", "label": "G #general",
                        "start_time": None, "end_time": None,
                        "before_cursor": "9", "scanned": 10, "deleted": 4,
                        "failed": 0, "skipped": 1}, root=tmp_path)
    answers = iter(["1", "1", "", "y"])  # server, channel, resume (default), confirm
    _run_server_deletion(monkeypatch, answers, tool, tmp_path)
    assert tool.fetch_calls == ["9"]  # resumed after the cursor, no rescan
    out = capsys.readouterr().out
    assert "UNFINISHED DELETION RUN" in out
    assert "Deleted 4 of your messages so far" in out


def test_server_deletion_resume_declined_starts_new_run(monkeypatch, tmp_path):
    tool = _deletion_tool(monkeypatch, [_msg(5)])
    save_session("c1", {"channel_id": "c1", "before_cursor": "9",
                        "scanned": 10, "deleted": 4}, root=tmp_path)
    answers = iter(["1", "1", "2", "", "", "y"])  # ..., new run, start, end, confirm
    _run_server_deletion(monkeypatch, answers, tool, tmp_path)
    assert tool.fetch_calls == [None]  # fresh scan from the top
    assert tool.delete_calls == ["5"]


def test_dry_run_reports_without_deleting(monkeypatch, tmp_path, capsys):
    tool = _deletion_tool(monkeypatch, [_msg(5, content="hello there")])
    monkeypatch.setattr("message_guillotine.deletion.sessions_root",
                        lambda: tmp_path)
    answers = iter(["1", "1", "", "", "y"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    cli.handle_server_deletion(tool, dry_run=True)
    assert tool.delete_calls == []
    out = capsys.readouterr().out
    assert "DRY RUN" in out
    assert "would be deleted" in out
    assert "hello there" in out  # sample quote
