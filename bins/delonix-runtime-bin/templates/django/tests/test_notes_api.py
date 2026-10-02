"""The HTTP contract through Django's test client: happy path, every error
class, paging, body limit, request ids."""

from __future__ import annotations

import json

import pytest

from notes import services

pytestmark = pytest.mark.django_db
NOTES = "/api/v1/notes"


def post(client, body, **headers):
    raw = body if isinstance(body, (bytes, str)) else json.dumps(body)
    return client.post(NOTES, data=raw, content_type="application/json", headers=headers)


def assert_error(response, status, code):
    assert response.status_code == status, response.content
    err = response.json()["error"]
    assert err["code"] == code
    assert err["message"]
    assert err["request_id"] == response["X-Request-ID"]
    return err


def test_create_get_and_list(client):
    created = post(client, {"title": "hello", "body": "world"})
    assert created.status_code == 201
    note = created.json()
    assert set(note) == {"id", "title", "body", "created_at"}
    assert created["Location"] == f"{NOTES}/{note['id']}"
    assert client.get(f"{NOTES}/{note['id']}").json() == note
    page = client.get(NOTES, {"offset": 0, "limit": 20}).json()
    assert page == {"items": [note], "total": 1, "offset": 0, "limit": 20}


def test_list_defaults_and_pages(client):
    for i in range(3):
        post(client, {"title": f"n{i}"})
    page = client.get(NOTES, {"limit": 2, "offset": 1}).json()
    assert [n["title"] for n in page["items"]] == ["n1", "n0"]
    assert (page["total"], page["offset"], page["limit"]) == (3, 1, 2)
    assert client.get(NOTES).json()["limit"] == 20


def test_validation_failed_lists_the_fields(client):
    err = assert_error(post(client, {"title": ""}), 422, "validation_failed")
    assert err["fields"] == {"title": "is required"}


@pytest.mark.parametrize(
    "body",
    [
        "{not json",
        '["a list"]',
        '{"title": "x", "unknown": 1}',
        '{"title": "x", "title": "y"}',
        '{"title": NaN}',
        b'{"title": "\xff"}',
        "",
    ],
)
def test_malformed_json_is_refused(client, body):
    assert_error(post(client, body), 400, "malformed_json")


def test_body_over_the_limit_is_413(client, settings):
    settings.DATA_UPLOAD_MAX_MEMORY_SIZE = 1024
    assert_error(post(client, {"title": "x", "body": "b" * 2000}), 413, "body_too_large")


@pytest.mark.parametrize(
    ("query", "field"),
    [
        ({"limit": 0}, "limit"),
        ({"limit": 101}, "limit"),
        ({"offset": -1}, "offset"),
        ({"limit": "ten"}, "limit"),
        ({"offset": "1_0"}, "offset"),
    ],
)
def test_bad_paging_is_422(client, query, field):
    assert field in assert_error(client.get(NOTES, query), 422, "validation_failed")["fields"]


def test_unknown_note_and_unknown_route_are_404(client):
    assert_error(client.get(f"{NOTES}/does-not-exist"), 404, "not_found")
    assert_error(client.get("/api/v1/nothing-here"), 404, "not_found")


def test_wrong_method_is_405_with_allow(client):
    response = client.delete(NOTES)
    assert_error(response, 405, "method_not_allowed")
    assert response["Allow"] == "GET, POST"


def test_internal_error_hides_the_stack(client, monkeypatch):
    def boom(**kwargs):
        raise RuntimeError("secret detail")

    monkeypatch.setattr(services, "list_notes", boom)
    response = client.get(NOTES)
    err = assert_error(response, 500, "internal")
    assert "secret detail" not in response.content.decode()
    assert err["message"] == "internal error"


def test_request_id_is_kept_when_plain_and_replaced_otherwise(client):
    assert client.get(NOTES, headers={"X-Request-ID": "abc-123"})["X-Request-ID"] == "abc-123"
    replaced = client.get(NOTES, headers={"X-Request-ID": "bad id\n"})["X-Request-ID"]
    assert replaced != "bad id\n" and len(replaced) == 16


def test_disallowed_host_is_400_in_the_error_shape(client):
    assert_error(client.get(NOTES, headers={"Host": "evil.example"}), 400, "bad_request")
