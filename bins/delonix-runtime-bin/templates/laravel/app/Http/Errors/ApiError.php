<?php

namespace App\Http\Errors;

use RuntimeException;

/** A transport-level refusal with its contract code (see ErrorRenderer). */
final class ApiError extends RuntimeException
{
    /** @param array<string, string>|null $fields */
    public function __construct(
        public readonly int $status,
        public readonly string $errorCode,
        string $message,
        public readonly ?array $fields = null,
    ) {
        parent::__construct($message);
    }

    public static function malformedJson(string $message): self
    {
        return new self(400, 'malformed_json', $message);
    }

    public static function bodyTooLarge(): self
    {
        return new self(413, 'body_too_large', 'request body too large');
    }

    public static function notFound(string $message = 'not found'): self
    {
        return new self(404, 'not_found', $message);
    }
}
