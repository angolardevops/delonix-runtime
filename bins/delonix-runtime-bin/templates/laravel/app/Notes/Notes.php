<?php

namespace App\Notes;

/**
 * The capability as its callers see it (HTTP controllers, the inbound
 * webhook). Telemetry decorates this interface (App\Telemetry\TracedNotes);
 * NoteService is the implementation.
 */
interface Notes
{
    /** @throws InvalidNote */
    public function create(string $title, ?string $body): Note;

    /** @throws NoteNotFound */
    public function get(string $id): Note;

    public function list(int $offset, int $limit): NotePage;
}
