<?php

namespace App\Notes;

/**
 * Port: tells the outside world a note exists. Implemented by App\Webhooks
 * (a queued, signed webhook) or by NullPublisher when no target is configured.
 * It must not block on the network: the use case is not the delivery.
 */
interface NotePublisher
{
    public function noteCreated(Note $note): void;
}
