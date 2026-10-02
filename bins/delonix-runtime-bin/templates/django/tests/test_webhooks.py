"""Inbound webhooks: signature, replay window, de-duplication, limits."""

from __future__ import annotations

import json
import time

import pytest

from notes.models import Note
from webhooks import signature
from webhooks.models import InboundDelivery

pytestmark = pytest.mark.django_db
KEY = b"k" * 32
URL = "/api/v1/webhooks/inbound"


@pytest.fixture
def inbound(app_config):
    app_config(webhook_inbound_key=KEY)


def deliver(client, body, msg_id="msg_1", ts=None, key=KEY, headers=None):
    raw = body if isinstance(body, bytes) else json.dumps(body).encode()
    ts = int(time.time()) if ts is None else ts
    hdrs = {
        signature.HEADER_ID: msg_id,
        signature.HEADER_TIMESTAMP: str(ts),
        signature.HEADER_SIGNATURE: signature.sign(key, msg_id, ts, raw),
    }
    if headers is not None:
        hdrs = headers
    return client.post(URL, data=raw, content_type="application/json", headers=hdrs)


def test_sign_and_verify_round_trip_and_rotation():
    sig = signature.sign(KEY, "id", 100, b"{}")
    signature.verify(KEY, "id", "100", sig, b"{}", now=100)
    signature.verify(KEY, "id", "100", "v1,AAAA " + sig, b"{}", now=100)  # several signatures
    with pytest.raises(signature.BadSignature):
        signature.verify(KEY, "id", "100", sig, b"{ }", now=100)  # one byte differs
    with pytest.raises(signature.BadTimestamp):
        signature.verify(KEY, "id", "100", sig, b"{}", now=100 + 301)
    with pytest.raises(signature.MissingHeaders):
        signature.verify(KEY, "", "100", sig, b"{}", now=100)


def test_endpoint_is_off_without_a_secret(client):
    response = deliver(client, {"type": "ping"})
    assert (response.status_code, response.json()["error"]["code"]) == (404, "not_found")


def test_ping_is_accepted(client, inbound):
    assert deliver(client, {"type": "ping"}).status_code == 204


def test_note_create_runs_the_use_case_once_per_id(client, inbound):
    event = {"type": "note.create", "data": {"title": "from a webhook"}}
    assert deliver(client, event, msg_id="msg_a").status_code == 204
    assert deliver(client, event, msg_id="msg_a").status_code == 204  # retry: acknowledged
    assert Note.objects.filter(title="from a webhook").count() == 1


def test_a_failed_delivery_is_not_recorded_so_its_retry_is_processed(client, inbound):
    assert deliver(client, {"type": "note.create", "data": {"title": ""}}, msg_id="msg_b").status_code == 422
    assert not InboundDelivery.objects.filter(webhook_id="msg_b").exists()
    ok = deliver(client, {"type": "note.create", "data": {"title": "fixed"}}, msg_id="msg_b")
    assert ok.status_code == 204


@pytest.mark.parametrize(
    ("kwargs", "status", "code"),
    [
        ({"key": b"w" * 32}, 401, "invalid_signature"),
        ({"ts": int(time.time()) - 600}, 401, "invalid_signature"),
        ({"headers": {}}, 400, "missing_signature"),
    ],
)
def test_refused_deliveries(client, inbound, kwargs, status, code):
    response = deliver(client, {"type": "ping"}, **kwargs)
    assert (response.status_code, response.json()["error"]["code"]) == (status, code)


def test_unknown_event_and_bad_json(client, inbound):
    assert deliver(client, {"type": "note.delete"}).json()["error"]["code"] == "unknown_event"
    assert deliver(client, b"{nope").json()["error"]["code"] == "malformed_json"


def test_body_limit(client, inbound):
    response = deliver(client, {"type": "ping", "pad": "x" * 300_000})
    assert (response.status_code, response.json()["error"]["code"]) == (413, "body_too_large")
