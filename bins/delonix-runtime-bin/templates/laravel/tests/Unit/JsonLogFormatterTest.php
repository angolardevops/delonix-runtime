<?php

namespace Tests\Unit;

use App\Logging\JsonLogFormatter;
use DateTimeImmutable;
use Monolog\Level;
use Monolog\LogRecord;
use PHPUnit\Framework\TestCase;

final class JsonLogFormatterTest extends TestCase
{
    public function test_one_flat_json_line_with_the_shared_keys(): void
    {
        $formatter = new JsonLogFormatter(['service' => 'svc', 'version' => '1.2.3', 'environment' => 'test']);
        $line = $formatter->format(new LogRecord(
            new DateTimeImmutable('2026-01-02T03:04:05.678901+00:00'),
            'app',
            Level::Info,
            'request',
            ['status' => 201, 'route' => '/api/v1/notes'],
            ['request_id' => 'req-1', 'trace_id' => str_repeat('a', 32), 'span_id' => str_repeat('b', 16)],
        ));

        $this->assertStringEndsWith("\n", $line);
        $this->assertSame(1, substr_count($line, "\n"));
        $this->assertSame([
            'time' => '2026-01-02T03:04:05.678901+00:00',
            'level' => 'info',
            'msg' => 'request',
            'service' => 'svc',
            'version' => '1.2.3',
            'environment' => 'test',
            'request_id' => 'req-1',
            'trace_id' => str_repeat('a', 32),
            'span_id' => str_repeat('b', 16),
            'status' => 201,
            'route' => '/api/v1/notes',
        ], json_decode($line, true));
    }

    public function test_keys_naming_secrets_are_redacted_at_any_depth(): void
    {
        $formatter = new JsonLogFormatter;
        $line = $formatter->format(new LogRecord(new DateTimeImmutable, 'app', Level::Warning, 'x', [
            'Authorization' => 'Bearer abc',
            'webhook_signature' => 'v1,zzz',
            'nested' => ['api_key' => 'k', 'password' => 'p', 'kept' => 'visible'],
            'webhook_id' => 'msg_1',
        ]));
        $decoded = json_decode($line, true);

        $this->assertSame('[REDACTED]', $decoded['Authorization']);
        $this->assertSame('[REDACTED]', $decoded['webhook_signature']);
        $this->assertSame(['api_key' => '[REDACTED]', 'password' => '[REDACTED]', 'kept' => 'visible'], $decoded['nested']);
        $this->assertSame('msg_1', $decoded['webhook_id']);
        $this->assertStringNotContainsString('Bearer abc', $line);
    }
}
