<?php

namespace App\Persistence;

use App\Models\Note as NoteRow;
use App\Notes\Note;
use App\Notes\NotePage;
use App\Notes\NoteRepository;
use DateTimeZone;

/**
 * Adapter of the NoteRepository port over Eloquent (SQLite by default; any
 * driver Laravel supports works). The mapping row <-> Note lives here and
 * nowhere else.
 */
final class EloquentNoteRepository implements NoteRepository
{
    public function save(Note $note): void
    {
        NoteRow::query()->create([
            'id' => $note->id,
            'title' => $note->title,
            'body' => $note->body,
            'created_at' => $note->createdAt->setTimezone(new DateTimeZone('UTC')),
        ]);
    }

    public function find(string $id): ?Note
    {
        $row = NoteRow::query()->find($id);

        return $row === null ? null : $this->toNote($row);
    }

    public function page(int $offset, int $limit): NotePage
    {
        $total = NoteRow::query()->count();
        $items = NoteRow::query()
            ->orderByDesc('created_at')
            ->orderByDesc('id')
            ->offset($offset)
            ->limit($limit)
            ->get()
            ->map(fn (NoteRow $row): Note => $this->toNote($row))
            ->values()
            ->all();

        return new NotePage($items, $total, $offset, $limit);
    }

    private function toNote(NoteRow $row): Note
    {
        return new Note($row->id, $row->title, $row->body, $row->created_at->toDateTimeImmutable());
    }
}
