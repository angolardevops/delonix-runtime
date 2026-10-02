<?php

namespace Tests\Unit;

use App\Support\ConfigValidator;
use PHPUnit\Framework\TestCase;

final class ConfigValidatorTest extends TestCase
{
    private const KEY = 'base64:2fl+Ktvkfl+Fuz4Qp/A75G2RTiWVA/ZoKZvp6fiiM10=';

    private const SECRET = 'whsec_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=';

    /**
     * @param  array<string, mixed>  $webhooks
     * @return array<string, mixed>
     */
    private function service(array $webhooks = [], string $maxBody = '1048576', string $shutdown = '15s'): array
    {
        return [
            'http' => ['max_body_bytes' => $maxBody],
            'shutdown_timeout' => $shutdown,
            'drain_delay' => '0s',
            'webhooks' => $webhooks + ['inbound_secret' => '', 'target_url' => '', 'target_secret' => ''],
            'otel' => ['endpoint' => '', 'timeout_ms' => '2000'],
        ];
    }

    public function test_the_defaults_are_valid_in_development(): void
    {
        $this->assertSame([], ConfigValidator::errors(['env' => 'development', 'key' => '', 'debug' => false], $this->service(), 'info'));
    }

    public function test_production_refuses_the_development_shortcuts(): void
    {
        $errors = ConfigValidator::errors(
            ['env' => 'production', 'key' => '', 'debug' => true],
            $this->service(['target_url' => 'http://receiver.example/hook', 'target_secret' => self::SECRET]),
            'info',
        );

        $this->assertCount(3, $errors);
        $this->assertStringContainsString('APP_KEY: required in production', $errors[0]);
        $this->assertStringContainsString('APP_DEBUG', $errors[1]);
        $this->assertStringContainsString('https only', $errors[2]);
    }

    public function test_a_valid_production_configuration_passes(): void
    {
        $this->assertSame([], ConfigValidator::errors(
            ['env' => 'production', 'key' => self::KEY, 'debug' => false],
            $this->service(['inbound_secret' => self::SECRET, 'target_url' => 'https://receiver.example/hook', 'target_secret' => self::SECRET]),
            'warning',
        ));
    }

    public function test_every_problem_is_listed_at_once_and_no_secret_value_is_echoed(): void
    {
        $errors = ConfigValidator::errors(
            ['env' => 'staging', 'key' => 'not-a-key', 'debug' => false],
            $this->service(['inbound_secret' => 'hunter2', 'target_url' => 'ftp://x', 'target_secret' => ''], maxBody: 'lots', shutdown: 'soon'),
            'verbose',
        );
        $text = implode("\n", $errors);

        foreach (['APP_ENV', 'LOG_LEVEL', 'APP_KEY', 'HTTP_MAX_BODY_BYTES', 'SHUTDOWN_TIMEOUT', 'WEBHOOK_INBOUND_SECRET', 'WEBHOOK_TARGET_URL', 'WEBHOOK_TARGET_SECRET'] as $name) {
            $this->assertStringContainsString($name.':', $text);
        }
        $this->assertStringNotContainsString('hunter2', $text);
        $this->assertStringNotContainsString('not-a-key', $text);
    }
}
