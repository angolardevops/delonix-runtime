<?php

namespace App\Notes;

use DateTimeImmutable;

/**
 * A note as the capability knows it: no persistence, no transport. The only
 * way to make a new one is {@see Note::create()}, which enforces the rules.
 */
final readonly class Note
{
    public const TITLE_MAX = 200;

    public const BODY_MAX = 10_000;

    public function __construct(
        public string $id,
        public string $title,
        public string $body,
        public DateTimeImmutable $createdAt,
    ) {}

    /**
     * @throws InvalidNote with one message per offending field
     */
    public static function create(string $title, ?string $body, DateTimeImmutable $now): self
    {
        $title = trim($title);
        $body ??= '';
        $fields = [];
        if ($title === '') {
            $fields['title'] = 'is required';
        } elseif (mb_strlen($title) > self::TITLE_MAX) {
            $fields['title'] = 'must be at most '.self::TITLE_MAX.' characters';
        }
        if (mb_strlen($body) > self::BODY_MAX) {
            $fields['body'] = 'must be at most '.self::BODY_MAX.' characters';
        }
        if ($fields !== []) {
            throw new InvalidNote($fields);
        }

        return new self(bin2hex(random_bytes(16)), $title, $body, $now);
    }
}
