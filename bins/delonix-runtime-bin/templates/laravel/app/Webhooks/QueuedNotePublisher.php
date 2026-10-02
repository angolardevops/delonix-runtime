<?php

namespace App\Webhooks;

use App\Jobs\DeliverWebhook;
use App\Notes\Note;
use App\Notes\NotePublisher;
use OpenTelemetry\API\Trace\Propagation\TraceContextPropagator;

/**
 * Adapter of the NotePublisher port: queues a signed `note.created` for the
 * worker. The note id is the webhook-id, so the receiver can de-duplicate
 * our retries. The current trace context travels in the job so the
 * delivery joins the request's trace.
 */
final class QueuedNotePublisher implements NotePublisher
{
    public function noteCreated(Note $note): void
    {
        $body = json_encode([
            'type' => 'note.created',
            'timestamp' => $note->createdAt->format(DATE_RFC3339_EXTENDED),
            'data' => [
                'id' => $note->id,
                'title' => $note->title,
                'body' => $note->body,
                'created_at' => $note->createdAt->format(DATE_RFC3339_EXTENDED),
            ],
        ], JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE);

        $carrier = [];
        TraceContextPropagator::getInstance()->inject($carrier);

        // afterCommit: never deliver an event for a row that was rolled back.
        DeliverWebhook::dispatch($note->id, $body, $carrier)->afterCommit();
    }
}
