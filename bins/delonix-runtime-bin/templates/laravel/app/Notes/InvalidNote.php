<?php

namespace App\Notes;

use DomainException;

/** A rule of the capability was broken; the transport maps it to 422. */
final class InvalidNote extends DomainException
{
    /** @param array<string, string> $fields field => reason */
    public function __construct(public readonly array $fields)
    {
        parent::__construct('invalid note');
    }
}
