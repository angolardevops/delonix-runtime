<?php

namespace App\Logging;

use Monolog\Formatter\JsonFormatter;
use Monolog\LogRecord;

/**
 * One JSON object per line, flat, with the keys every line of every service
 * shares: time, level, msg, service, version, environment, then request_id,
 * trace_id and span_id when they exist, then the event's own fields.
 * Values under keys that name secrets are replaced by "[REDACTED]".
 */
final class JsonLogFormatter extends JsonFormatter
{
    private const SENSITIVE = '/authorization|cookie|password|passwd|secret|token|signature|api_?key|app_key/i';

    /** @param array<string, string> $static service, version, environment */
    public function __construct(private readonly array $static = [])
    {
        parent::__construct(self::BATCH_MODE_NEWLINES, true, true, false);
    }

    public function format(LogRecord $record): string
    {
        $line = [
            'time' => $record->datetime->format('Y-m-d\TH:i:s.uP'),
            'level' => strtolower($record->level->getName()),
            'msg' => $record->message,
        ] + $this->static;

        /** @var array<string, mixed> $fields */
        $fields = $this->normalize(array_merge($record->extra, $record->context));
        foreach (self::redact($fields) as $key => $value) {
            $line[$key] ??= $value;
        }

        return $this->toJson($line, true)."\n";
    }

    /**
     * @param  array<array-key, mixed>  $fields
     * @return array<array-key, mixed>
     */
    public static function redact(array $fields): array
    {
        foreach ($fields as $key => $value) {
            if (is_string($key) && preg_match(self::SENSITIVE, $key) === 1) {
                $fields[$key] = '[REDACTED]';
            } elseif (is_array($value)) {
                $fields[$key] = self::redact($value);
            }
        }

        return $fields;
    }
}
