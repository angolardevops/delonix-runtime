<?php

namespace App\Webhooks;

use RuntimeException;

/**
 * A delivery failed verification. The code decides the HTTP answer
 * (400 for a malformed request, 401 for a bad signature); the message never
 * says which byte was wrong.
 */
final class InvalidWebhook extends RuntimeException
{
    private function __construct(string $message, public readonly string $errorCode, public readonly int $status)
    {
        parent::__construct($message);
    }

    public static function missingHeaders(): self
    {
        return new self('webhook headers are missing', 'missing_signature', 400);
    }

    public static function badTimestamp(): self
    {
        return new self('webhook signature is not valid', 'invalid_signature', 401);
    }

    public static function badSignature(): self
    {
        return new self('webhook signature is not valid', 'invalid_signature', 401);
    }

    public static function malformedBody(string $message): self
    {
        return new self($message, 'malformed_json', 400);
    }

    public static function unknownEvent(): self
    {
        return new self('unknown webhook event type', 'unknown_event', 422);
    }
}
