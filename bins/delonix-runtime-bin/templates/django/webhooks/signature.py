"""Standard Webhooks signatures (https://www.standardwebhooks.com).

Headers `webhook-id`, `webhook-timestamp` (Unix seconds) and
`webhook-signature: v1,<base64 HMAC-SHA256(key, "<id>.<timestamp>.<body>")>`.
A published format lets a sender or receiver written in any language (or an
existing library) talk to this service without reading its code.
"""

from __future__ import annotations

import base64
import hashlib
import hmac

HEADER_ID = "webhook-id"
HEADER_TIMESTAMP = "webhook-timestamp"
HEADER_SIGNATURE = "webhook-signature"

# How far a timestamp may be from now, either way: bounds how long a captured
# request can be replayed.
TOLERANCE_SECONDS = 5 * 60


class MissingHeaders(Exception):
    pass


class BadTimestamp(Exception):
    pass


class BadSignature(Exception):
    pass


def sign(key: bytes, msg_id: str, timestamp: int, body: bytes) -> str:
    """The webhook-signature header value."""
    mac = hmac.new(key, f"{msg_id}.{timestamp}.".encode() + body, hashlib.sha256)
    return "v1," + base64.b64encode(mac.digest()).decode()


def verify(key: bytes, msg_id: str, timestamp: str, signatures: str, body: bytes, now: float) -> None:
    """Check a delivery over the RAW body bytes (re-serialised JSON has other
    bytes and would break every signature). Raises MissingHeaders (→ 400),
    BadTimestamp or BadSignature (→ 401); neither says which byte was wrong."""
    if not msg_id or not timestamp or not signatures:
        raise MissingHeaders()
    try:
        ts = int(timestamp)
    except ValueError:
        raise BadTimestamp() from None
    if abs(now - ts) > TOLERANCE_SECONDS:
        raise BadTimestamp()
    expected = sign(key, msg_id, ts, body).encode()
    # Several space-separated signatures are allowed (key rotation).
    if not any(hmac.compare_digest(sig.encode(), expected) for sig in signatures.split()):
        raise BadSignature()
