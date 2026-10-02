"""HTTP tests through the whole stack (middleware, handlers, error mapping)."""

from __future__ import annotations

from collections.abc import Iterator

from fastapi import FastAPI, HTTPException
from fastapi.testclient import TestClient

from __MODULE__.app import (
    create_app,
)
from __MODULE__.lifecycle import (
    Readiness,
)

from .conftest import JsonLines, Spans, make_settings


def test_note_lifecycle(client: TestClient) -> None:
    created = client.post("/api/v1/notes", json={"title": " hello ", "body": "world"})
    assert created.status_code == 201
    note = created.json()
    assert note["title"] == "hello"
    assert created.headers["location"] == f"/api/v1/notes/{note['id']}"
    assert note["created_at"].endswith("Z")

    assert client.get(f"/api/v1/notes/{note['id']}").json() == note
    client.post("/api/v1/notes", json={"title": "second"})
    page = client.get("/api/v1/notes", params={"limit": 1}).json()
    assert page["total"] == 2
    assert page["limit"] == 1
    assert [n["title"] for n in page["items"]] == ["second"]


def _error(resp_json: dict[str, object]) -> dict[str, object]:
    err = resp_json["error"]
    assert isinstance(err, dict)
    assert err["request_id"]
    return err


def test_errors_share_one_shape(client: TestClient) -> None:
    cases = [
        (client.post("/api/v1/notes", json={"title": ""}), 422, "validation_failed"),
        (client.post("/api/v1/notes", json={"body": "no title"}), 422, "validation_failed"),
        (client.post("/api/v1/notes", json={"title": "t", "extra": 1}), 422, "validation_failed"),
        (client.get("/api/v1/notes", params={"limit": 0}), 422, "validation_failed"),
        (client.get("/api/v1/notes", params={"limit": "x"}), 422, "validation_failed"),
        (
            client.post(
                "/api/v1/notes", content=b"{nope", headers={"content-type": "application/json"}
            ),
            400,
            "malformed_json",
        ),
        (client.post("/api/v1/notes", json=["not", "an", "object"]), 400, "malformed_json"),
        (client.get("/api/v1/notes/does-not-exist"), 404, "not_found"),
        (client.get("/no/such/route"), 404, "not_found"),
        (client.delete("/api/v1/notes"), 405, "method_not_allowed"),
    ]
    for resp, status, code in cases:
        assert resp.status_code == status, (resp.request.url, resp.text)
        assert _error(resp.json())["code"] == code, resp.text


def test_validation_names_the_fields(client: TestClient) -> None:
    resp = client.post("/api/v1/notes", json={"title": "", "body": "b" * 10_001})
    assert _error(resp.json())["fields"] == {
        "title": "is required",
        "body": "must be at most 10000 characters",
    }


def test_body_limit_answers_413_announced_or_streamed(spans: Spans) -> None:
    app = create_app(make_settings(http_max_body_bytes=64), spans.telemetry)
    with TestClient(app) as client:
        big = {"title": "x" * 100}
        assert client.post("/api/v1/notes", json=big).status_code == 413

        def chunks() -> Iterator[bytes]:  # no Content-Length: chunked
            yield b'{"title": "'
            yield b"x" * 100
            yield b'"}'

        resp = client.post(
            "/api/v1/notes", content=chunks(), headers={"content-type": "application/json"}
        )
        assert resp.status_code == 413
        assert _error(resp.json())["code"] == "body_too_large"


def test_unexpected_error_is_a_bare_500(spans: Spans, logs: JsonLines) -> None:
    app = create_app(make_settings(), spans.telemetry)

    @app.get("/boom")
    async def boom() -> None:
        raise RuntimeError("secret detail")

    with TestClient(app, raise_server_exceptions=False) as client:
        resp = client.get("/boom")
    assert resp.status_code == 500
    assert _error(resp.json())["code"] == "internal"
    assert "secret detail" not in resp.text
    # The detail is in the log line, findable by the request id of the response.
    (line,) = [entry for entry in logs.lines if entry["msg"] == "unhandled error serving request"]
    assert line["request_id"] == resp.headers["x-request-id"]
    assert "secret detail" in line["error"]


def test_http_exception_keeps_the_shape(spans: Spans) -> None:
    app = create_app(make_settings(), spans.telemetry)

    @app.get("/teapot")
    async def teapot() -> None:
        raise HTTPException(status_code=418, detail="short and stout")

    with TestClient(app) as client:
        err = _error(client.get("/teapot").json())
    assert err["message"] == "short and stout"


def test_request_id_is_echoed_when_plain_and_replaced_otherwise(client: TestClient) -> None:
    kept = client.get("/api/v1/health/live", headers={"x-request-id": "abc-123"})
    assert kept.headers["x-request-id"] == "abc-123"
    replaced = client.get("/api/v1/health/live", headers={"x-request-id": "bad\x7fid"})
    assert replaced.headers["x-request-id"] != "bad\x7fid"


def test_readiness_follows_the_lifecycle(spans: Spans) -> None:
    app: FastAPI = create_app(make_settings(), spans.telemetry)
    readiness: Readiness = app.state.readiness
    # Without `with`, the lifespan has not run: the service is starting.
    starting = TestClient(app).get("/api/v1/health/ready")
    assert (starting.status_code, starting.json()) == (503, {"status": "starting"})
    with TestClient(app) as client:
        assert client.get("/api/v1/health/ready").json() == {"status": "ready"}
        assert client.get("/api/v1/health/live").status_code == 200
        readiness.begin_drain(timeout=5)
        draining = client.get("/api/v1/health/ready")
        assert (draining.status_code, draining.json()) == (503, {"status": "draining"})
        # Draining is not dead: the service still answers what it is sent.
        assert client.get("/api/v1/health/live").status_code == 200


def test_api_docs_are_off_in_production(spans: Spans) -> None:
    prod = create_app(make_settings(app_env="production"), spans.telemetry)
    with TestClient(prod) as client:
        assert client.get("/docs").status_code == 404
        assert client.get("/openapi.json").status_code == 404
