<?php

namespace App\Http\Requests;

use App\Notes\NoteService;
use Illuminate\Foundation\Http\FormRequest;

/** Query of GET /api/v1/notes: offset >= 0, limit 1..100 (default 20). */
final class ListNotesRequest extends FormRequest
{
    /** @return array<string, list<string>> */
    public function rules(): array
    {
        return [
            'offset' => ['sometimes', 'integer', 'min:0'],
            'limit' => ['sometimes', 'integer', 'min:1', 'max:'.NoteService::MAX_LIMIT],
        ];
    }

    public function offset(): int
    {
        return (int) $this->query('offset', '0');
    }

    public function limit(): int
    {
        return (int) $this->query('limit', (string) NoteService::DEFAULT_LIMIT);
    }
}
