<?php

namespace App\Http\Requests;

use App\Notes\Note;
use Illuminate\Foundation\Http\FormRequest;
use Illuminate\Validation\Validator;

/**
 * Input of POST /api/v1/notes. Titles arrive trimmed (Laravel's TrimStrings
 * middleware), an empty title becomes null and fails `required`. Unknown
 * fields are refused, so a typo ("titel") is an error, not a silent drop.
 * The capability re-checks the same rules (App\Notes\Note::create) for the
 * callers that do not come through HTTP.
 */
final class StoreNoteRequest extends FormRequest
{
    private const ALLOWED = ['title', 'body'];

    /** @return array<string, list<string>> */
    public function rules(): array
    {
        return [
            'title' => ['required', 'string', 'max:'.Note::TITLE_MAX],
            'body' => ['nullable', 'string', 'max:'.Note::BODY_MAX],
        ];
    }

    /** @return list<callable(Validator): void> */
    public function after(): array
    {
        return [function (Validator $validator): void {
            foreach (array_keys($this->all()) as $key) {
                if (! in_array($key, self::ALLOWED, true)) {
                    $validator->errors()->add((string) $key, 'is not allowed');
                }
            }
        }];
    }
}
