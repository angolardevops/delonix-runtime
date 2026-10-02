# 0002 — Standard Webhooks, the database queue, de-duplication by unique key

Status: accepted (template default)

## Context
The service both receives and sends events. A home-made signature scheme
would force every peer to read this code. Laravel has a native queue with
retries and backoff; PHP answers each request in a process that must not
wait for a remote receiver.

## Decision
- Format: Standard Webhooks (HMAC-SHA256 over `id.timestamp.body`, `whsec_`
  secrets, 5-minute tolerance), verified over `$request->getContent()`.
- Outbound: a `DeliverWebhook` job on Laravel's `database` queue (the `jobs`
  table), dispatched after commit, run by `php artisan queue:work` in a
  second container. 4 attempts, full-jitter backoff, 5 s per HTTP attempt;
  429/5xx/network errors are retried, other 4xx are final (`failed_jobs`).
  The note id is the `webhook-id`, the receiver's idempotency key.
- Inbound de-duplication: a row per accepted `webhook-id` in
  `webhook_receipts` (primary key), inserted in the same transaction as the
  event's effect.

## Alternatives
- **Deliver inside the request** (`Http::retry()`): the caller would wait for
  our receiver, and a crash loses the event.
- **Redis queue / Horizon**: better throughput and visibility; another
  service to run. Switch `QUEUE_CONNECTION` when volume justifies it.
- **Cache-based de-duplication** (`Cache::add`): lost on restart with the
  default store and not transactional with the effect.
- **spatie/laravel-webhook-server / -client**: mature packages with their
  own signature header; not the Standard Webhooks format.

## Trade-off
Delivery is at-least-once: the job row survives restarts, a worker killed
mid-delivery leaves the job reserved and it runs again after `retry_after`
(90 s), so a receiver can see the same `webhook-id` twice. It is never
exactly-once. The note and its job are two statements, not one transaction:
a crash between them loses that event (wrap both in `DB::transaction` in
`NoteService`'s caller if that matters). `webhook_receipts` grows without
bound; prune rows older than the replay window on a schedule.
