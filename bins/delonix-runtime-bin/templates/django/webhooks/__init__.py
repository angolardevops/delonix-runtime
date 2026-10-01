"""Webhooks in both directions, in the Standard Webhooks format
(https://www.standardwebhooks.com):

    signature.py    sign/verify: HMAC-SHA256 over "<id>.<timestamp>.<raw body>"
    inbound.py      handle a verified delivery: de-duplicate, run the use case
    views.py        POST /api/v1/webhooks/inbound
    dispatcher.py   outbound delivery: queue, background thread, retries
    receivers.py    note_created → dispatcher (connected in apps.py)
"""
