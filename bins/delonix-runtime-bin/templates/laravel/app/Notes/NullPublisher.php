<?php

namespace App\Notes;

/** Publishes nothing: used when WEBHOOK_TARGET_URL is not set. */
final class NullPublisher implements NotePublisher
{
    public function noteCreated(Note $note): void {}
}
