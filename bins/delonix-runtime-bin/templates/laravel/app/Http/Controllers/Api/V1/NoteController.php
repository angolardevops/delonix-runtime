<?php

namespace App\Http\Controllers\Api\V1;

use App\Http\Requests\ListNotesRequest;
use App\Http\Requests\StoreNoteRequest;
use App\Http\Resources\NoteResource;
use App\Notes\Notes;
use Illuminate\Http\JsonResponse;

/**
 * HTTP transport of the notes capability. It decodes, calls the use case
 * through the Notes interface, and encodes. Errors are thrown and turned
 * into the one error shape by App\Http\Errors\ErrorRenderer.
 */
final class NoteController
{
    public function __construct(private readonly Notes $notes) {}

    public function store(StoreNoteRequest $request): JsonResponse
    {
        $note = $this->notes->create((string) $request->validated('title'), $request->validated('body'));

        return (new NoteResource($note))
            ->response()
            ->setStatusCode(201)
            ->header('Location', '/api/v1/notes/'.$note->id);
    }

    public function index(ListNotesRequest $request): JsonResponse
    {
        $page = $this->notes->list($request->offset(), $request->limit());

        return new JsonResponse([
            'items' => array_map(NoteResource::fromNote(...), $page->items),
            'total' => $page->total,
            'offset' => $page->offset,
            'limit' => $page->limit,
        ]);
    }

    public function show(string $id): JsonResponse
    {
        return new JsonResponse(NoteResource::fromNote($this->notes->get($id)));
    }
}
