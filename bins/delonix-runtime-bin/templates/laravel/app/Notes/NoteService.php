<?php

namespace App\Notes;

use DateTimeImmutable;

/**
 * The use cases. Depends on its two ports and nothing else: no HTTP, no
 * Eloquent, no facades, no telemetry (tests/Architecture enforces it).
 */
final class NoteService implements Notes
{
    public const DEFAULT_LIMIT = 20;

    public const MAX_LIMIT = 100;

    public function __construct(
        private readonly NoteRepository $notes,
        private readonly NotePublisher $publisher,
    ) {}

    public function create(string $title, ?string $body): Note
    {
        $note = Note::create($title, $body, new DateTimeImmutable);
        $this->notes->save($note);
        // Queued, not delivered: a slow receiver never slows this request.
        $this->publisher->noteCreated($note);

        return $note;
    }

    public function get(string $id): Note
    {
        return $this->notes->find($id) ?? throw new NoteNotFound($id);
    }

    public function list(int $offset, int $limit): NotePage
    {
        $offset = max(0, $offset);
        $limit = max(1, min(self::MAX_LIMIT, $limit));

        return $this->notes->page($offset, $limit);
    }
}
