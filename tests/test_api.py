"""Platform API client logic with a stubbed HTTP session."""

import pytest

from message_guillotine.api import HistoryFetchError, MessageGuillotine


class FakeResponse:
    def __init__(self, status_code=200, json_data=None, headers=None, text=""):
        self.status_code = status_code
        self._json = json_data if json_data is not None else []
        self.headers = headers or {}
        self.text = text

    def json(self):
        return self._json


class FakeSession:
    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = []

    def get(self, url, params=None, timeout=None):
        self.calls.append((url, params))
        return self.responses.pop(0)

    def update(self, headers):
        pass


def make_tool(responses):
    tool = MessageGuillotine.__new__(MessageGuillotine)  # no real session/headers
    tool.session = FakeSession(responses)
    tool.current_user = {"id": "7", "username": "me", "discriminator": "0001"}
    tool.dm_channels = []
    tool.guilds = []
    return tool


def api_message(i, author_id="7"):
    return {"id": str(i),
            "author": {"id": author_id},
            "timestamp": f"2026-01-0{i}T00:00:00+00:00"}


def test_get_guild_channels_filters_to_text_types():
    tool = make_tool([FakeResponse(json_data=[
        {"id": "1", "type": 0},    # text
        {"id": "2", "type": 2},    # voice, dropped
        {"id": "3", "type": 4},    # category, dropped
        {"id": "4", "type": 5},    # news, kept
    ])])
    assert [c["id"] for c in tool.get_guild_channels("99")] == ["1", "4"]


def test_get_guild_channels_empty_on_error():
    tool = make_tool([FakeResponse(status_code=403)])
    assert tool.get_guild_channels("99") == []


def test_get_all_dm_users_dedupes_and_excludes_self_and_bots():
    tool = make_tool([])
    tool.dm_channels = [
        {"id": "1", "recipients": [
            {"id": "9", "username": "bob", "discriminator": "0001", "global_name": "Bob"},
            {"id": "9", "username": "bob-dup", "discriminator": "0002", "global_name": ""},
        ]},
        {"id": "2", "recipients": [
            {"id": "7", "username": "me", "discriminator": "0001", "global_name": ""},
            {"id": "8", "username": "robot", "bot": True, "discriminator": "0000", "global_name": ""},
        ]},
    ]
    users = tool.get_all_dm_users()
    assert len(users) == 1  # deduped by id; self and bots excluded
    assert users[0]["id"] == "9"
    assert users[0]["username"] == "bob-dup"  # later recipients overwrite earlier ones
    assert users[0]["channel_id"] == "1"


def test_fetch_page_fails_fast_on_auth_error():
    tool = make_tool([FakeResponse(status_code=401, text="nope")])
    with pytest.raises(HistoryFetchError):
        tool._fetch_page_with_retries("https://api.example", {})


def test_fetch_page_retries_on_rate_limit():
    tool = make_tool([
        FakeResponse(status_code=429, headers={"Retry-After": "1"}),
        FakeResponse(status_code=200, json_data=[]),
    ])
    resp = tool._fetch_page_with_retries("https://api.example", {})
    assert resp.status_code == 200


def test_iter_history_before_pages_newest_first():
    page1 = [api_message(5), api_message(4), api_message(3)]
    page2 = [api_message(2), api_message(1)]
    tool = make_tool([FakeResponse(json_data=page1), FakeResponse(json_data=page2)])
    ids = [m["id"] for m in tool.iter_channel_history_before("5", limit=3)]
    assert ids == ["5", "4", "3", "2", "1"]  # newest first throughout
    assert tool.session.calls[0][1] == {"limit": 3}  # no cursor on the first page
    assert tool.session.calls[1][1] == {"limit": 3, "before": "3"}


def test_iter_history_before_starts_at_cursor():
    tool = make_tool([FakeResponse(json_data=[])])
    list(tool.iter_channel_history_before("5", before_id="99"))
    assert tool.session.calls[0][1] == {"limit": 100, "before": "99"}


def test_iter_history_before_sleeps_only_between_pages(monkeypatch):
    import time as time_mod
    sleeps = []
    monkeypatch.setattr(time_mod, "sleep", lambda s: sleeps.append(s))
    full_page = [api_message(i) for i in range(3, 0, -1)]
    short_page = [api_message(1)]
    tool = make_tool([
        FakeResponse(json_data=full_page),
        FakeResponse(json_data=short_page),
    ])
    tool.limit = None
    list(tool.iter_channel_history_before("5", limit=3))
    assert sleeps == [0.5]  # between pages, none after the short final page


def test_count_my_messages_returns_total_results():
    tool = make_tool([FakeResponse(status_code=200,
                                    json_data={"total_results": 42, "messages": []})])
    assert tool.count_my_messages("42", "7", min_id="100", max_id="900") == 42
    url, params = tool.session.calls[0]
    assert url == "https://discord.com/api/v10/channels/42/messages/search"
    assert params == {"author_id": "7", "min_id": "100", "max_id": "900"}


def test_count_my_messages_omits_unset_bounds():
    tool = make_tool([FakeResponse(status_code=200, json_data={"total_results": 0})])
    assert tool.count_my_messages("42", "7") == 0
    assert tool.session.calls[0][1] == {"author_id": "7"}


def test_count_my_messages_none_when_search_unavailable():
    tool = make_tool([FakeResponse(status_code=404, text="search disabled")])
    assert tool.count_my_messages("42", "7") is None


def test_count_my_messages_none_on_bad_body():
    tool = make_tool([FakeResponse(status_code=200, json_data={"messages": []})])
    assert tool.count_my_messages("42", "7") is None


def test_count_my_messages_none_on_network_error():
    import requests

    class ExplodingSession:
        def get(self, url, params=None, timeout=None):
            raise requests.RequestException("boom")

    tool = MessageGuillotine.__new__(MessageGuillotine)
    tool.session = ExplodingSession()
    assert tool.count_my_messages("42", "7") is None
