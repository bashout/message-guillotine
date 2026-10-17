"""Archive loading, filtering, search, and payload builders."""

import json
import os

from message_guillotine.viewer import archive as viewer_archive
from tests.conftest import attachment_entry, make_archive, message_record


def load(folder):
    return viewer_archive.Archive(folder)


def test_meta_payload(archive):
    meta = load(archive).meta_payload()
    assert meta["chat"] == "tester"
    assert meta["channel_id"] == "42"
    assert meta["total_messages"] == 5
    assert meta["total_attachments"] == 1
    assert meta["first_timestamp"].startswith("2026-01-01")
    assert meta["last_timestamp"].startswith("2026-01-05")


def test_page_payload_ascending(archive):
    payload = load(archive).page_payload(0, 2)
    assert [m["id"] for m in payload["messages"]] == ["1", "2"]
    assert payload["total"] == 5


def test_page_payload_reverse(archive):
    payload = load(archive).page_payload(0, 2, reverse=True)
    assert [m["id"] for m in payload["messages"]] == ["5", "4"]


def test_page_payload_negative_start_counts_from_end(archive):
    payload = load(archive).page_payload(-2, 2)
    assert [m["id"] for m in payload["messages"]] == ["4", "5"]
    assert payload["start"] == 3


def test_page_payload_seq_is_position(archive):
    payload = load(archive).page_payload(0, 5)
    assert [m["seq"] for m in payload["messages"]] == [0, 1, 2, 3, 4]


def test_filtered_view_date_range(archive):
    viewer = load(archive)
    # bare date_to excludes that day lexically (parse_filter_params extends it)
    assert list(viewer.filtered_view(("2026-01-02", "2026-01-03", False, False, False))) == [1]
    end_of_day = "2026-01-03T23:59:59.999999+00:00"
    assert list(viewer.filtered_view(("2026-01-02", end_of_day, False, False, False))) == [1, 2]


def test_filtered_view_has_attachment_and_link(archive):
    viewer = load(archive)
    assert list(viewer.filtered_view((None, None, True, False, False))) == [3]
    assert list(viewer.filtered_view((None, None, False, True, False))) == [2]


def test_filtered_view_pinned(tmp_path):
    records = [
        message_record(1),
        {**message_record(2), "pinned": True},
        message_record(3),
    ]
    viewer = load(make_archive(tmp_path, records=records))
    assert list(viewer.filtered_view((None, None, False, False, True))) == [1]


def test_anchor_payload(archive):
    payload = load(archive).anchor_payload("3", 3)
    assert payload["anchor_index"] == 2
    assert [m["id"] for m in payload["messages"]] == ["2", "3", "4"]


def test_anchor_payload_unknown_id(archive):
    assert load(archive).anchor_payload("99", 3) is None


def test_resolve_payload_truncates_content(archive):
    resolved = load(archive).resolve_payload(["5"])
    assert len(resolved["5"]["content"]) == 140
    assert resolved["5"]["index"] == 4


def test_resolve_payload_unknown_ids_skipped(archive):
    resolved = load(archive).resolve_payload(["1", "99"])
    assert set(resolved) == {"1"}
    assert resolved["1"]["index"] == 0


def test_search_payload_matches_content(archive):
    payload = load(archive).search_payload("HELLO")  # case-insensitive
    assert len(payload["results"]) == 3  # messages 1, 2, 4 hold "hello"
    assert payload["results"][0]["id"] == "4"  # most recent match first
    assert payload["truncated"] is False


def test_search_payload_author_filter(archive):
    assert load(archive).search_payload("hello", author="bob")["results"] == []


def test_search_payload_truncates_at_limit(archive):
    payload = load(archive).search_payload("hello", limit=2)
    assert len(payload["results"]) == 2
    assert payload["truncated"] is True


def test_attachment_view_only_when_file_exists(archive):
    viewer = load(archive)
    view = viewer.attachment_view("a1")
    assert view["url"] == "/media/0001_4_pic.png"
    assert view["size"] == 9
    os.remove(archive / "attachments" / "0001_4_pic.png")
    assert load(archive).attachment_view("a1") is None  # reload drops the missing file


def test_attachment_year_counts(archive):
    assert load(archive).attachment_year_counts() == [{"year": "2026", "count": 1}]


def test_attachments_payload_year_filter(archive):
    viewer = load(archive)
    payload = viewer.attachments_payload(0, 10, year="2026")
    assert payload["total"] == 1
    assert payload["attachments"][0]["message_index"] == 3
    assert viewer.attachments_payload(0, 10, year="1999")["total"] == 0


def test_stats_payload(archive):
    stats = load(archive).stats_payload()
    assert stats["total_messages"] == 5
    assert stats["total_attachments"] == 1
    assert stats["edited"] == 1
    assert stats["replies"] == 1
    assert stats["pinned"] == 0  # the fixture archive has no pinned records
    assert stats["with_attachments"] == 1
    assert stats["authors"] == [{"name": "alice", "count": 5}]
    assert stats["months"] == [{"month": "2026-01", "count": 5}]
    assert stats["attachment_types"] == [{"ext": ".png", "count": 1}]


def test_resolve_refuses_traversal(archive):
    viewer = load(archive)
    assert viewer.resolve("0001_4_pic.png").name == "0001_4_pic.png"
    assert viewer.resolve("../meta.json") is None
    assert viewer.resolve("missing.png") is None


def test_parse_filter_params_empty_is_none():
    assert viewer_archive.parse_filter_params({}) is None


def test_parse_filter_params_bare_end_date_includes_whole_day():
    filters = viewer_archive.parse_filter_params({"from": ["2026-01-02"],
                                                  "to": ["2026-01-03"]})
    assert filters[0] == "2026-01-02"
    assert filters[1] == "2026-01-03T23:59:59.999999+00:00"


def test_parse_filter_params_flags():
    filters = viewer_archive.parse_filter_params({"has_attachment": ["true"],
                                                  "has_link": ["true"],
                                                  "pinned": ["true"]})
    assert filters == (None, None, True, True, True)
    assert viewer_archive.parse_filter_params({"pinned": ["true"]}) == (None, None, False, False, True)


def test_parse_filter_params_normalizes_offsets_to_utc():
    filters = viewer_archive.parse_filter_params(
        {"from": ["2026-01-02T12:00:00+02:00"], "to": ["2026-01-03T01:00:00Z"]})
    assert filters[0] == "2026-01-02T10:00:00+00:00"
    assert filters[1] == "2026-01-03T01:00:00+00:00"


def test_parse_filter_params_naive_timestamp_taken_as_utc():
    filters = viewer_archive.parse_filter_params({"from": ["2026-01-02T12:00:00"]})
    assert filters[0] == "2026-01-02T12:00:00+00:00"


def test_parse_filter_params_unparseable_passthrough():
    filters = viewer_archive.parse_filter_params({"from": ["nonsense"]})
    assert filters[0] == "nonsense"


def test_find_archives_requires_meta_and_messages(tmp_path):
    make_archive(tmp_path, name="good", records=[message_record(1)])
    bad = make_archive(tmp_path, name="no-messages")
    (bad / "messages.jsonl").unlink()
    assert [a["name"] for a in viewer_archive.find_archives(tmp_path)] == ["good"]


def test_find_archives_newest_first_and_chat_fallback(tmp_path):
    older = make_archive(tmp_path, name="older", records=[message_record(1)])
    newer = make_archive(tmp_path, name="newer", chat=None, records=[message_record(1)])
    os.utime(older, (1000000000, 1000000000))
    os.utime(newer, (2000000000, 2000000000))
    found = viewer_archive.find_archives(tmp_path)
    assert [a["name"] for a in found] == ["newer", "older"]
    assert found[0]["chat"] == "newer"  # falls back to folder name


def test_find_archives_missing_root(tmp_path):
    assert viewer_archive.find_archives(tmp_path / "nope") == []


def test_load_sorts_hard_killed_unsorted_archive(tmp_path):
    from tests.conftest import make_archive

    records = [message_record(1), message_record(3), message_record(2)]
    viewer = load(make_archive(tmp_path, records=records))
    assert [m["id"] for m in viewer.messages] == ["1", "2", "3"]
