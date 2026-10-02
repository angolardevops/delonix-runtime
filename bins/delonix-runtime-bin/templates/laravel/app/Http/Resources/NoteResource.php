<?php

namespace App\Http\Resources;

use App\Notes\Note;
use Illuminate\Http\Request;
use Illuminate\Http\Resources\Json\JsonResource;

/**
 * JSON form of a note (api/openapi.yaml, schema Note).
 *
 * @property Note $resource
 */
final class NoteResource extends JsonResource
{
    /** @return array<string, string> */
    public function toArray(Request $request): array
    {
        return self::fromNote($this->resource);
    }

    /** @return array<string, string> */
    public static function fromNote(Note $note): array
    {
        return [
            'id' => $note->id,
            'title' => $note->title,
            'body' => $note->body,
            'created_at' => $note->createdAt->setTimezone(new \DateTimeZone('UTC'))->format(DATE_RFC3339_EXTENDED),
        ];
    }
}
