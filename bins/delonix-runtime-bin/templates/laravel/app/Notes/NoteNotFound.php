<?php

namespace App\Notes;

use DomainException;

/** No note has that id; the transport maps it to 404. */
final class NoteNotFound extends DomainException
{
    public function __construct(public readonly string $id)
    {
        parent::__construct('note not found');
    }
}
