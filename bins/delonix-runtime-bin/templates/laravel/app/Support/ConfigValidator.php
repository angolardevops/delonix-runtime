<?php

namespace App\Support;

use App\Webhooks\StandardWebhook;
use InvalidArgumentException;

/**
 * Checks the whole configuration once, at startup, and lists EVERY problem
 * (not only the first). Production refuses the development shortcuts: no
 * APP_KEY, APP_DEBUG on, plain-http webhook targets.
 */
final class ConfigValidator
{
    public const ENVIRONMENTS = ['development', 'test', 'production'];

    public const LOG_LEVELS = ['debug', 'info', 'notice', 'warning', 'error'];

    /**
     * @param  array<string, mixed>  $app  config('app')
     * @param  array<string, mixed>  $service  config('service')
     * @return list<string> empty when the configuration is valid
     */
    public static function errors(array $app, array $service, string $logLevel): array
    {
        $errors = [];
        $env = (string) ($app['env'] ?? '');
        $production = $env === 'production';

        if (! in_array($env, self::ENVIRONMENTS, true)) {
            $errors[] = sprintf('APP_ENV: "%s" is not one of %s', $env, implode(', ', self::ENVIRONMENTS));
        }
        if (! in_array(strtolower($logLevel), self::LOG_LEVELS, true)) {
            $errors[] = sprintf('LOG_LEVEL: "%s" is not one of %s', $logLevel, implode(', ', self::LOG_LEVELS));
        }

        $key = (string) ($app['key'] ?? '');
        if ($key === '') {
            if ($production) {
                $errors[] = 'APP_KEY: required in production (php artisan key:generate --show, then store it as a secret)';
            }
        } elseif (! self::validAppKey($key)) {
            $errors[] = 'APP_KEY: must be "base64:" followed by 32 random bytes (php artisan key:generate --show)';
        }
        if ($production && filter_var($app['debug'] ?? false, FILTER_VALIDATE_BOOL)) {
            $errors[] = 'APP_DEBUG: must be false in production';
        }

        $max = (string) ($service['http']['max_body_bytes'] ?? '');
        if (! ctype_digit($max) || (int) $max < 1) {
            $errors[] = sprintf('HTTP_MAX_BODY_BYTES: "%s" is not a positive integer', $max);
        }
        foreach (['SHUTDOWN_TIMEOUT' => 'shutdown_timeout', 'DRAIN_DELAY' => 'drain_delay'] as $name => $field) {
            $value = (string) ($service[$field] ?? '');
            if (preg_match('/^\d+(ms|s|m)$/', $value) !== 1) {
                $errors[] = sprintf('%s: "%s" is not a duration like 15s, 500ms or 1m', $name, $value);
            }
        }

        $hooks = $service['webhooks'] ?? [];
        if (($hooks['inbound_secret'] ?? '') !== '') {
            $errors = [...$errors, ...self::secretErrors('WEBHOOK_INBOUND_SECRET', (string) $hooks['inbound_secret'])];
        }
        $target = (string) ($hooks['target_url'] ?? '');
        if ($target !== '') {
            $scheme = parse_url($target, PHP_URL_SCHEME);
            $host = parse_url($target, PHP_URL_HOST);
            if (! in_array($scheme, ['http', 'https'], true) || ! is_string($host) || $host === '') {
                $errors[] = 'WEBHOOK_TARGET_URL: not an http(s) URL';
            } elseif ($production && $scheme !== 'https') {
                $errors[] = 'WEBHOOK_TARGET_URL: production sends signed events over https only';
            }
            if (($hooks['target_secret'] ?? '') === '') {
                $errors[] = 'WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set';
            } else {
                $errors = [...$errors, ...self::secretErrors('WEBHOOK_TARGET_SECRET', (string) $hooks['target_secret'])];
            }
        }

        $otel = $service['otel'] ?? [];
        $endpoint = (string) ($otel['endpoint'] ?? '');
        if ($endpoint !== '' && ! in_array(parse_url($endpoint, PHP_URL_SCHEME), ['http', 'https'], true)) {
            $errors[] = 'OTEL_EXPORTER_OTLP_ENDPOINT: not an http(s) URL (the OTLP/HTTP port, usually 4318)';
        }
        $timeout = (string) ($otel['timeout_ms'] ?? '');
        if (! ctype_digit($timeout) || (int) $timeout < 1) {
            $errors[] = sprintf('OTEL_EXPORTER_OTLP_TIMEOUT: "%s" is not a positive number of milliseconds', $timeout);
        }

        return $errors;
    }

    private static function validAppKey(string $key): bool
    {
        if (! str_starts_with($key, 'base64:')) {
            return false;
        }
        $raw = base64_decode(substr($key, 7), true);

        return $raw !== false && strlen($raw) === 32;
    }

    /** @return list<string> */
    private static function secretErrors(string $name, string $secret): array
    {
        try {
            StandardWebhook::parseSecret($secret);

            return [];
        } catch (InvalidArgumentException $e) {
            return ["{$name}: ".$e->getMessage()];
        }
    }
}
