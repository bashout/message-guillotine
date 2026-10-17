"""The viewer HTTP server, end to end over loopback."""

import json
import threading
import urllib.error
import urllib.request

import pytest

from message_guillotine.viewer.server import ClientInputError, create_server, int_param
from tests.conftest import make_archive, message_record


@pytest.fixture
def server_url(archive):
    server, url = create_server(archive.parent, select_archive=archive.name)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield url
    server.shutdown()
    server.server_close()


def fetch(url, path, host="localhost"):
    """Request url+path; returns (status, content-type, body). HTTP errors are
    returned as statuses instead of raised."""
    request = urllib.request.Request(url + path, headers={"Host": host})
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status, response.headers.get("Content-Type"), response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers.get("Content-Type"), error.read()


def fetch_headers(url, path, host="localhost"):
    """Like fetch, but also returns the response headers."""
    request = urllib.request.Request(url + path, headers={"Host": host})
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status, response.headers.get("Content-Type"), response.read(), response.headers
    except urllib.error.HTTPError as error:
        return error.code, error.headers.get("Content-Type"), error.read(), error.headers


def test_serves_embedded_ui(server_url):
    status, ctype, body = fetch(server_url, "/")
    assert status == 200 and "text/html" in ctype and body.startswith(b"<!DOCTYPE html>")
    assert fetch(server_url, "/app.js")[1] == "application/javascript; charset=utf-8"
    assert fetch(server_url, "/style.css")[1] == "text/css; charset=utf-8"


def test_archives_listing_and_selected_name(server_url, archive):
    status, _, body = fetch(server_url, "/api/archives")
    payload = json.loads(body)
    assert status == 200
    assert payload["selected"] == archive.name
    assert payload["archives"][0]["chat"] == "tester"
    assert "mtime" not in payload["archives"][0]


def test_select_known_and_unknown(server_url, archive):
    assert fetch(server_url, f"/api/select?archive={archive.name}")[0] == 200
    assert fetch(server_url, "/api/select?archive=nope")[0] == 404
    assert fetch(server_url, "/api/select?archive=..")[0] == 404  # traversal rejected
    assert fetch(server_url, "/api/select")[0] == 400


def test_messages_paging_and_filters(server_url):
    _, _, body = fetch(server_url, "/api/messages?count=2")
    assert [m["id"] for m in json.loads(body)["messages"]] == ["1", "2"]
    _, _, body = fetch(server_url, "/api/messages?count=2&dir=desc")
    assert [m["id"] for m in json.loads(body)["messages"]] == ["5", "4"]
    _, _, body = fetch(server_url, "/api/messages?has_attachment=true")
    assert json.loads(body)["total"] == 1
    _, _, body = fetch(server_url, "/api/messages?has_link=true")
    assert json.loads(body)["total"] == 1
    _, _, body = fetch(server_url, "/api/messages?anchor=3&count=3")
    payload = json.loads(body)
    assert payload["anchor_index"] == 2
    assert [m["id"] for m in payload["messages"]] == ["2", "3", "4"]
    assert fetch(server_url, "/api/messages?count=abc")[0] == 400
    assert fetch(server_url, "/api/messages?anchor=99")[0] == 404


def test_search_endpoint(server_url):
    _, _, body = fetch(server_url, "/api/search?q=hello&author=alice")
    assert len(json.loads(body)["results"]) == 3
    assert fetch(server_url, "/api/search?q=")[0] == 200  # empty query, empty results


def test_stats_years_and_attachments(server_url):
    _, _, body = fetch(server_url, "/api/stats")
    assert json.loads(body)["total_messages"] == 5
    _, _, body = fetch(server_url, "/api/attachment-years")
    assert json.loads(body)["years"] == [{"year": "2026", "count": 1}]
    _, _, body = fetch(server_url, "/api/attachments?year=2026")
    assert json.loads(body)["total"] == 1
    _, _, body = fetch(server_url, "/api/attachments?year=1999")
    assert json.loads(body)["total"] == 0


def test_media_serving(server_url):
    status, ctype, body = fetch(server_url, "/media/0001_4_pic.png")
    assert status == 200 and "image/png" in ctype and body == b"\x89PNGdata"
    _, _, _, headers = fetch_headers(server_url, "/media/0001_4_pic.png")
    assert headers.get("Content-Disposition") == "inline"
    assert headers.get("X-Content-Type-Options") == "nosniff"


def test_media_unsafe_types_are_downloads(server_url, archive):
    # An html (or svg, xml, pdf, ...) attachment must not render inline in the
    # viewer's origin: it would run script with access to every archive.
    (archive / "attachments" / "evil.html").write_text("<script>fetch('/api/stats')</script>")
    status, ctype, body = fetch(server_url, "/media/evil.html")
    assert status == 200 and "text/html" in ctype
    _, _, _, headers = fetch_headers(server_url, "/media/evil.html")
    assert headers["Content-Disposition"].startswith("attachment")
    assert headers["Content-Security-Policy"] == "sandbox"
    _, _, _, headers = fetch_headers(server_url, "/media/0001_4_pic.png?download=1")
    assert headers["Content-Disposition"].startswith("attachment")


def test_head_requests_carry_headers_but_no_body(server_url):
    request = urllib.request.Request(server_url + "/", method="HEAD",
                                     headers={"Host": "localhost"})
    with urllib.request.urlopen(request, timeout=5) as response:
        assert response.status == 200
        assert int(response.headers["Content-Length"]) > 0
        assert response.read() == b""  # no body on HEAD


def test_archive_pin_keeps_tabs_independent(server_url, archive, tmp_path):
    from tests.conftest import make_archive, message_record

    other = make_archive(tmp_path, name="other", channel_id="43", chat="other-chat",
                         records=[message_record(1, content="other message")])
    # Both archives are addressable at once via ?archive=, without any
    # shared "selected" state flipping between them.
    _, _, body = fetch(server_url, f"/api/messages?archive={archive.name}")
    assert json.loads(body)["total"] == 5
    _, _, body = fetch(server_url, f"/api/messages?archive={other.name}")
    assert json.loads(body)["total"] == 1
    assert json.loads(body)["messages"][0]["content"] == "other message"
    assert fetch(server_url, "/api/messages?archive=nope")[0] == 404
    # The selection is untouched by pinned requests.
    _, _, body = fetch(server_url, "/api/archives")
    assert json.loads(body)["selected"] == archive.name


def test_media_traversal_refused(server_url):
    assert fetch(server_url, "/media/..%2Fmeta.json")[0] == 404
    assert fetch(server_url, "/media/missing.png")[0] == 404


def test_unknown_path_is_404(server_url):
    assert fetch(server_url, "/nope")[0] == 404


def test_non_local_host_refused(server_url):
    assert fetch(server_url, "/", host="evil.example.com")[0] == 403
    assert fetch(server_url, "/api/meta", host="192.168.1.5")[0] == 200  # IP literal is fine


def test_create_server_falls_back_to_newest_archive(tmp_path, archive):
    server, url = create_server(tmp_path)  # no explicit selection
    assert server and url.startswith("http://127.0.0.1:")
    selected = server.library.selected_name
    server.server_close()
    assert selected == archive.name  # only archive: newest by default


def test_int_param():
    assert int_param({"count": ["3"]}, "count", 7) == 3
    assert int_param({}, "count", 7) == 7
    assert int_param({"count": [None]}, "count", 7) == 7
    with pytest.raises(ClientInputError):
        int_param({"count": ["abc"]}, "count", 7)


def test_messages_pinned_filter_and_stats(tmp_path):
    records = [
        message_record(1),
        {**message_record(2), "pinned": True},
        message_record(3),
    ]
    folder = make_archive(tmp_path, records=records)
    server, url = create_server(tmp_path, select_archive=folder.name)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        _, _, body = fetch(url, "/api/messages?pinned=true")
        payload = json.loads(body)
        assert payload["total"] == 1
        assert payload["messages"][0]["id"] == "2"
        assert payload["messages"][0]["pinned"] is True
        _, _, body = fetch(url, "/api/stats")
        assert json.loads(body)["pinned"] == 1
    finally:
        server.shutdown()
        server.server_close()
