<?php

namespace App\Webhooks;

use App\Models\WebhookReceipt;
use App\Notes\InvalidNote;
use App\Notes\Notes;
use Illuminate\Database\UniqueConstraintViolationException;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Log;

/**
 * Handles one VERIFIED inbound delivery. The receipt (unique webhook-id) and
 * the effect are written in one transaction: either both exist or neither,
 * so a delivery that failed can be retried with the same id, and one that
 * succeeded is acknowledged again without running twice.
 */
final class InboundProcessor
{
    public function __construct(private readonly Notes $notes) {}

    /**
     * @return bool false when the id was already processed (a duplicate)
     *
     * @throws InvalidWebhook for a body that is not a known event
     * @throws InvalidNote for a note.create that breaks the rules
     */
    public function process(string $webhookId, string $rawBody): bool
    {
        try {
            $event = json_decode($rawBody, true, 16, JSON_THROW_ON_ERROR);
        } catch (\JsonException) {
            throw InvalidWebhook::malformedBody('webhook body is not JSON');
        }
        if (! is_array($event) || ! is_string($event['type'] ?? null)) {
            throw InvalidWebhook::malformedBody('webhook body has no event type');
        }

        try {
            DB::transaction(function () use ($webhookId, $event): void {
                WebhookReceipt::query()->create(['webhook_id' => $webhookId, 'received_at' => now()]);
                match ($event['type']) {
                    'ping' => null,
                    'note.create' => $this->createNote($event['data'] ?? null),
                    default => throw InvalidWebhook::unknownEvent(),
                };
            });
        } catch (UniqueConstraintViolationException) {
            Log::info('webhook duplicate ignored', ['webhook_id' => $webhookId]);

            return false;
        }
        Log::info('webhook accepted', ['webhook_id' => $webhookId, 'event' => $event['type']]);

        return true;
    }

    private function createNote(mixed $data): void
    {
        if (! is_array($data) || ! is_string($data['title'] ?? '') || ! is_string($data['body'] ?? '')) {
            throw InvalidWebhook::malformedBody('webhook data is not a note');
        }
        $this->notes->create((string) ($data['title'] ?? ''), $data['body'] ?? null);
    }
}
