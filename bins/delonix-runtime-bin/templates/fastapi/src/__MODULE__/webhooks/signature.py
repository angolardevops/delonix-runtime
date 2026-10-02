"""Sign and verify Standard Webhooks: headers `webhook-id`, `webhook-timestamp`
and `webhook-signature`, the signature being
`v1,` + base64(HMAC-SHA256(key, "<id>.<timestamp>.<raw body>")).

A published format lets a peer written in another language, or an existing
library, talk to this service without reading its code. Standard library only.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import hmac

HEADER_ID = "webhook-id"
HEADER_TIMESTAMP = "webhook-timestamp"
HEADER_SIGNATURE = "webhook-signature"

#: How far a timestamp may be from now, either way (seconds). Bounds how long a
#: captured request can be replayed.
TOLERANCE_SECONDS = 5 * 60
_PREFIX = "whsec_"
_MIN_KEY_BYTES = 24


class WebhookError(Exception):
    """Base of the verification errors. None of them says which byte was wrong."""


class MissingHeadersError(WebhookError):
    pass


class BadTimestampError(WebhookError):
    pass


class BadSignatureError(WebhookError):
    pass


def parse_secret(secret: str) -> bytes:
    """Decode a `whsec_<base64>` secret; the key must be at least 24 bytes."""
    if not secret.startswith(_PREFIX):
        raise ValueError('webhook secret must look like "whsec_<base64>"')
    try:
        key = base64.b64decode(secret.removeprefix(_PREFIX), validate=True)
    except binascii.Error as exc:
        raise ValueError("webhook secret is not valid base64") from exc
    if len(key) < _MIN_KEY_BYTES:
        raise ValueError(f"webhook secret must decode to at least {_MIN_KEY_BYTES} bytes")
    return key


def sign(key: bytes, msg_id: str, timestamp: int, body: bytes) -> str:
    """The `webhook-signature` header value."""
    mac = hmac.new(key, f"{msg_id}.{timestamp}.".encode() + body, hashlib.sha256)
    return "v1," + base64.b64encode(mac.digest()).decode()


def verify(
    key: bytes, msg_id: str, timestamp: str, signatures: str, body: bytes, now: float
) -> None:
    """Check a delivery over the RAW body bytes (never a re-encoded body:
    re-serialising JSON changes bytes and breaks every signature)."""
    if not msg_id or not timestamp or not signatures:
        raise MissingHeadersError("missing webhook headers")
    try:
        ts = int(timestamp)
    except ValueError as exc:
        raise BadTimestampError("webhook timestamp is not an integer") from exc
    if abs(now - ts) > TOLERANCE_SECONDS:
        raise BadTimestampError("webhook timestamp outside the tolerance window")
    expected = sign(key, msg_id, ts, body)
    # Several space-separated signatures are allowed (key rotation).
    for candidate in signatures.split():
        if hmac.compare_digest(candidate.encode(), expected.encode()):
            return
    raise BadSignatureError("no webhook signature matches")
