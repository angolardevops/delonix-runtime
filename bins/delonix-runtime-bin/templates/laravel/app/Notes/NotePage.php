<?php

namespace App\Notes;

/** One page of notes, newest first. */
final readonly class NotePage
{
    /** @param list<Note> $items */
    public function __construct(
        public array $items,
        public int $total,
        public int $offset,
        public int $limit,
    ) {}
}
