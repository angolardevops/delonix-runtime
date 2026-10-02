<?php

namespace App\Webhooks;

use InvalidArgumentException;

/**
 * Signing and verification in the Standard Webhooks format
 * (https://www.standardwebhooks.com): headers webhook-id, webhook-timestamp
 * and webhook-signature = "v1," + base64(HMAC-SHA256(key, "<id>.<ts>.<raw body>")).
 * A published format lets peers in any language talk to this service.
 */
final class StandardWebhook
{
    public const HEADER_ID = 'webhook-id';

    public const HEADER_TIMESTAMP = 'webhook-timestamp';

    public const HEADER_SIGNATURE = 'webhook-signature';

    /** Seconds a timestamp may be off, either way: bounds replay of a captured request. */
    public const TOLERANCE = 300;

    /**
     * Decodes a "whsec_<base64>" secret into the raw key (at least 24 bytes).
     *
     * @throws InvalidArgumentException naming what is wrong, never the value
     */
    public static function parseSecret(string $secret): string
    {
        if (! str_starts_with($secret, 'whsec_')) {
            throw new InvalidArgumentException('webhook secret must look like "whsec_<base64>"');
        }
        $key = base64_decode(substr($secret, 6), true);
        if ($key === false) {
            throw new InvalidArgumentException('webhook secret is not valid base64');
        }
        if (strlen($key) < 24) {
            throw new InvalidArgumentException('webhook secret must decode to at least 24 bytes');
        }

        return $key;
    }

    public static function sign(string $key, string $id, int $timestamp, string $body): string
    {
        return 'v1,'.base64_encode(hash_hmac('sha256', "{$id}.{$timestamp}.{$body}", $key, true));
    }

    /**
     * Verifies over the RAW bytes: a re-encoded JSON body changes bytes and
     * breaks every signature. Several signatures may be sent (key rotation);
     * one match is enough. Comparison is constant-time.
     *
     * @throws InvalidWebhook
     */
    public static function verify(string $key, ?string $id, ?string $timestamp, ?string $signatures, string $body, int $now): void
    {
        if ($id === null || $id === '' || $timestamp === null || $timestamp === '' || $signatures === null || $signatures === '') {
            throw InvalidWebhook::missingHeaders();
        }
        if (! ctype_digit($timestamp) || abs($now - (int) $timestamp) > self::TOLERANCE) {
            throw InvalidWebhook::badTimestamp();
        }
        $expected = self::sign($key, $id, (int) $timestamp, $body);
        foreach (preg_split('/\s+/', trim($signatures)) ?: [] as $candidate) {
            if (hash_equals($expected, $candidate)) {
                return;
            }
        }
        throw InvalidWebhook::badSignature();
    }
}
