"""Archive writing: filenames, manifest compaction, and archive_chat runs."""

import json

import pytest

from message_guillotine import archive_format, archiver
from message_guillotine.api import HistoryFetchError
from tests.conftest import make_archive


def api_message(i, attachments=()):
    return {
        "id": str(i),
        "author": {"id": "7", "username": "alice", "global_name": "Alice"},
        "timestamp": f"2026-01-{i:02d}T00:00:00+00:00",
        "content": f"msg {i}",
        "attachments": [
            {"id": f"a{i}", "filename": f"pic{i}.png", "url": f"https://cdn.example/{i}"}
        ] if attachments else [],
    }


class FakeTool:
    """MessageGuillotine stand-in: yields canned history, no network."""

    def __init__(self, messages, fail_urls=(), fail_on=None):
        self.messages = sorted(messages, key=lambda m: int(m["id"]))
        self.fail_urls = set(fail_urls)
        self.fail_on = fail_on  # message id after which history fetch fails

    def download_attachment(self, url, dest_path):
        if url in self.fail_urls:
            return None
        dest_path.write_bytes(b"data")
        return 4

    def iter_channel_history(self, channel_id, after_id="0"):
        for msg in self.messages:
            if int(msg["id"]) > int(after_id):
                if self.fail_on is not None and int(msg["id"]) > self.fail_on:
                    raise HistoryFetchError("boom")
                yield msg


def records_of(folder):
    with open(folder / "messages.jsonl", encoding="utf-8") as f:
        return [json.loads(line) for line in f]


def manifest_of(folder):
    with open(folder / "manifest.jsonl", encoding="utf-8") as f:
        return [json.loads(line) for line in f]


def test_sanitize_filename():
    assert archiver.sanitize_filename("My Chat!.png") == "My_Chat_.png"
    assert archiver.sanitize_filename("  ..  ") == "file"  # empty after strip
    assert archiver.sanitize_filename("ok.txt") == "ok.txt"
    assert archiver.sanitize_filename("") == "file"


def test_compact_manifest_latest_wins(tmp_path):
    folder = make_archive(tmp_path, manifest=[
        {"attachment_id": "a1", "seq": 1, "status": "failed", "local_path": None},
        {"attachment_id": "a1", "seq": 1, "status": "ok", "local_path": "attachments/x"},
        {"attachment_id": "a2", "seq": 2, "status": "ok", "local_path": "attachments/y"},
    ])
    entries = archiver.compact_manifest(folder)
    assert len(entries) == 2
    assert entries[0] == {"attachment_id": "a1", "seq": 1, "status": "ok",
                          "local_path": "attachments/x"}
    assert entries[1]["attachment_id"] == "a2"
    # the rewritten manifest holds exactly the compacted entries
    assert manifest_of(folder) == entries
    assert not (folder / "manifest.jsonl.part").exists()


def test_archive_chat_new_run(tmp_path):
    tool = FakeTool([api_message(1), api_message(2, attachments=[True]), api_message(3)])
    folder = archiver.archive_chat(tool, "42", "My Chat", tmp_path / "archives")
    assert folder.parent == tmp_path / "archives"
    assert folder.name.startswith("My_Chat-archive-")
    meta = json.loads((folder / "meta.json").read_text(encoding="utf-8"))
    assert meta == {"channel_id": "42", "chat": "My Chat"}
    assert [r["id"] for r in records_of(folder)] == ["1", "2", "3"]  # oldest first
    entries = manifest_of(folder)
    assert len(entries) == 1 and entries[0]["status"] == "ok"
    assert (folder / entries[0]["local_path"]).is_file()
    assert archive_format.read_archive_status(folder) is True


def test_archive_chat_resumes_without_duplicates(tmp_path):
    archive_dir = tmp_path / "archives"
    tool = FakeTool([api_message(1), api_message(2), api_message(3)])
    first = archiver.archive_chat(tool, "42", "chat", archive_dir)
    second = archiver.archive_chat(tool, "42", "chat", archive_dir)
    assert second == first
    assert [r["id"] for r in records_of(second)] == ["1", "2", "3"]


def test_archive_chat_retries_failed_attachment(tmp_path):
    archive_dir = tmp_path / "archives"
    messages = [api_message(1, attachments=[True])]
    failing = FakeTool(messages, fail_urls=["https://cdn.example/1"])
    folder = archiver.archive_chat(failing, "42", "chat", archive_dir)
    assert manifest_of(folder)[0]["status"] == "failed"

    ok = FakeTool(messages)
    folder = archiver.archive_chat(ok, "42", "chat", archive_dir)
    entries = manifest_of(folder)
    assert len(entries) == 1 and entries[0]["status"] == "ok"
    assert (folder / entries[0]["local_path"]).is_file()


def test_archive_chat_marks_incomplete_on_fetch_failure(tmp_path):
    from message_guillotine import archive_format

    archive_dir = tmp_path / "archives"
    tool = FakeTool([api_message(1), api_message(2), api_message(3)], fail_on=1)
    assert archiver.archive_chat(tool, "42", "chat", archive_dir) is None  # failed run
    folder = archive_format.find_existing_archive("42", archive_dir)
    assert archive_format.read_archive_status(folder) is False
    assert [r["id"] for r in records_of(folder)] == ["1"]  # pre-failure writes kept

    # a rerun with a healthy tool resumes and completes the archive
    folder = archiver.archive_chat(FakeTool(tool.messages), "42", "chat", archive_dir)
    assert archive_format.read_archive_status(folder) is True
    assert [r["id"] for r in records_of(folder)] == ["1", "2", "3"]


def test_archive_chat_marks_incomplete_on_interrupt_and_resumes(tmp_path):
    from message_guillotine import archive_format

    class InterruptedTool(FakeTool):
        def iter_channel_history(self, channel_id, after_id="0"):
            for msg in self.messages:
                if int(msg["id"]) > int(after_id):
                    yield msg
            raise KeyboardInterrupt  # Ctrl-C mid-run

    archive_dir = tmp_path / "archives"
    tool = InterruptedTool([api_message(1), api_message(2)])
    assert archiver.archive_chat(tool, "42", "chat", archive_dir) is None
    folder = archive_format.find_existing_archive("42", archive_dir)
    assert archive_format.read_archive_status(folder) is False
    assert [r["id"] for r in records_of(folder)] == ["1", "2"]  # writes kept

    # a rerun with a healthy tool resumes and completes the archive
    folder = archiver.archive_chat(FakeTool(tool.messages), "42", "chat", archive_dir)
    assert archive_format.read_archive_status(folder) is True
    assert [r["id"] for r in records_of(folder)] == ["1", "2"]


def test_archive_chat_separate_folder_per_channel(tmp_path):
    archive_dir = tmp_path / "archives"
    first = archiver.archive_chat(FakeTool([api_message(1)]), "42", "chat-a", archive_dir)
    second = archiver.archive_chat(FakeTool([api_message(1)]), "43", "chat-b", archive_dir)
    assert second != first
    meta = json.loads((second / "meta.json").read_text(encoding="utf-8"))
    assert meta["channel_id"] == "43"


def test_archive_chat_non_ascii_label_falls_back_to_channel_id(tmp_path):
    # Cyrillic/emoji labels all sanitize to nothing: the channel id keeps
    # different chats in different folders instead of jumbling them together.
    archive_dir = tmp_path / "archives"
    first = archiver.archive_chat(FakeTool([api_message(1)]), "1001", "Дорогой", archive_dir)
    second = archiver.archive_chat(FakeTool([api_message(1)]), "1002", "Дорогая", archive_dir)
    assert first != second
    assert first.name.startswith("chat-1001-archive-")
    assert second.name.startswith("chat-1002-archive-")


def test_archive_chat_same_name_same_day_no_clobber(tmp_path):
    # Two different ASCII chats that sanitize to the same name on the same
    # day must not overwrite each other's meta.json.
    archive_dir = tmp_path / "archives"
    first = archiver.archive_chat(FakeTool([api_message(1)]), "42", "My Chat", archive_dir)
    second = archiver.archive_chat(FakeTool([api_message(1)]), "43", "My Chat!", archive_dir)
    assert second != first
    assert json.loads((first / "meta.json").read_text(encoding="utf-8"))["channel_id"] == "42"
    assert json.loads((second / "meta.json").read_text(encoding="utf-8"))["channel_id"] == "43"


def test_attachment_filename_falls_back_to_attachment_id(tmp_path):
    messages = [{
        "id": "1",
        "author": {"id": "7", "username": "alice", "global_name": "Alice"},
        "timestamp": "2026-01-01T00:00:00+00:00",
        "content": "msg 1",
        "attachments": [{"id": "a9", "filename": "картинка",
                         "url": "https://cdn.example/1"}],
    }]
    folder = archiver.archive_chat(FakeTool(messages), "42", "chat", tmp_path / "archives")
    saved = [p.name for p in (folder / "attachments").iterdir()]
    assert saved == ["0001_1_att-a9"]  # GUID fallback, not a shared "file" name


def test_archive_chat_merges_into_existing_archive(tmp_path):
    archive_dir = tmp_path / "archives"
    target = archiver.archive_chat(FakeTool([api_message(10), api_message(11)]),
                                    "42", "chat-a", archive_dir)
    tool = FakeTool([api_message(5), api_message(9, attachments=[True]),
                     api_message(12)])
    folder = archiver.archive_chat(tool, "43", "chat-b", archive_dir,
                                   merge_target=target)
    assert folder == target
    # merged records are id-sorted (ids are chronological), not appended raw
    assert [r["id"] for r in records_of(folder)] == ["5", "9", "10", "11", "12"]
    entries = manifest_of(folder)
    assert [e["attachment_id"] for e in entries] == ["a9"]
    assert (folder / entries[0]["local_path"]).is_file()
    meta = json.loads((folder / "meta.json").read_text(encoding="utf-8"))
    assert meta["channel_ids"] == ["42", "43"]
    assert meta["channel_cursors"] == {"42": "11", "43": "12"}
    assert archive_format.read_archive_status(folder) is True


def test_merged_archive_resumes_each_channel_incrementally(tmp_path):
    archive_dir = tmp_path / "archives"
    target = archiver.archive_chat(FakeTool([api_message(10), api_message(11)]),
                                    "42", "chat-a", archive_dir)
    archiver.archive_chat(FakeTool([api_message(5), api_message(12)]), "43",
                          "chat-b", archive_dir, merge_target=target)

    # re-archiving the merged-in channel resumes it and appends only the new message
    folder = archiver.archive_chat(FakeTool([api_message(5), api_message(12),
                                            api_message(20)]),
                                   "43", "chat-b", archive_dir)
    assert folder == target
    assert [r["id"] for r in records_of(folder)] == ["5", "10", "11", "12", "20"]
    cursors = archive_format.read_channel_cursors(folder)
    assert cursors["43"] == "20"

    # re-archiving the original channel resumes from its own cursor too
    folder = archiver.archive_chat(FakeTool([api_message(10), api_message(11),
                                            api_message(15)]),
                                   "42", "chat-a", archive_dir)
    assert folder == target
    assert [r["id"] for r in records_of(folder)] == ["5", "10", "11", "12", "15", "20"]


def test_archive_chat_merge_target_must_be_an_archive(tmp_path):
    folder = archiver.archive_chat(FakeTool([api_message(1)]), "42", "chat",
                                   tmp_path, merge_target=tmp_path / "missing")
    assert folder is None
