<?php

namespace Tests\Unit;

use App\Webhooks\InvalidWebhook;
use App\Webhooks\StandardWebhook;
use InvalidArgumentException;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;

final class StandardWebhookTest extends TestCase
{
    private const NOW = 1_700_000_000;

    private string $key;

    protected function setUp(): void
    {
        $this->key = str_repeat("\x01", 32);
    }

    public function test_matches_the_reference_vector_of_the_specification(): void
    {
        // https://www.standardwebhooks.com — the example of the spec.
        $key = StandardWebhook::parseSecret('whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw');
        $signature = StandardWebhook::sign($key, 'msg_p5jXN8AQM9LWM0D4loKWxJek', 1614265330, '{"test": 2432232314}');

        $this->assertSame('v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=', $signature);
    }

    public function test_a_signed_delivery_verifies(): void
    {
        $signature = StandardWebhook::sign($this->key, 'msg_1', self::NOW, '{"type":"ping"}');
        StandardWebhook::verify($this->key, 'msg_1', (string) self::NOW, $signature, '{"type":"ping"}', self::NOW + 10);
        // Key rotation: any one of several signatures is enough.
        StandardWebhook::verify($this->key, 'msg_1', (string) self::NOW, 'v1,AAAA '.$signature, '{"type":"ping"}', self::NOW);
        $this->addToAssertionCount(2);
    }

    /** @return array<string, array{0: ?string, 1: ?string, 2: string, 3: string, 4: int, 5: string}> */
    public static function refusals(): array
    {
        $key = str_repeat("\x01", 32);
        $good = StandardWebhook::sign($key, 'msg_1', self::NOW, 'body');

        return [
            'missing id' => [null, (string) self::NOW, $good, 'body', self::NOW, 'missing_signature'],
            'missing timestamp' => ['msg_1', null, $good, 'body', self::NOW, 'missing_signature'],
            'body changed by one byte' => ['msg_1', (string) self::NOW, $good, 'body ', self::NOW, 'invalid_signature'],
            'another id' => ['msg_2', (string) self::NOW, $good, 'body', self::NOW, 'invalid_signature'],
            'too old (replay)' => ['msg_1', (string) self::NOW, $good, 'body', self::NOW + 301, 'invalid_signature'],
            'from the future' => ['msg_1', (string) self::NOW, $good, 'body', self::NOW - 301, 'invalid_signature'],
            'timestamp not a number' => ['msg_1', 'yesterday', $good, 'body', self::NOW, 'invalid_signature'],
            'signed with another key' => ['msg_1', (string) self::NOW, StandardWebhook::sign(str_repeat("\x02", 32), 'msg_1', self::NOW, 'body'), 'body', self::NOW, 'invalid_signature'],
        ];
    }

    #[DataProvider('refusals')]
    public function test_refuses(?string $id, ?string $timestamp, string $signature, string $body, int $now, string $code): void
    {
        try {
            StandardWebhook::verify($this->key, $id, $timestamp, $signature, $body, $now);
            $this->fail('expected InvalidWebhook');
        } catch (InvalidWebhook $e) {
            $this->assertSame($code, $e->errorCode);
        }
    }

    #[DataProvider('badSecrets')]
    public function test_a_secret_must_have_the_standard_form(string $secret): void
    {
        $this->expectException(InvalidArgumentException::class);
        StandardWebhook::parseSecret($secret);
    }

    /** @return array<string, array{0: string}> */
    public static function badSecrets(): array
    {
        return [
            'no prefix' => [base64_encode(str_repeat('k', 32))],
            'not base64' => ['whsec_***'],
            'too short' => ['whsec_'.base64_encode('short')],
        ];
    }
}
