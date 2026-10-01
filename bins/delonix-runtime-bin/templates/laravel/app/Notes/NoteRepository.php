<?php

namespace App\Notes;

/** Port: where notes are kept. Implemented by App\Persistence. */
interface NoteRepository
{
    public function save(Note $note): void;

    public function find(string $id): ?Note;

    /** Newest first. */
    public function page(int $offset, int $limit): NotePage;
}
