<?php

// The service's own configuration. Every value comes from the environment
// (see .env.example); App\Support\ConfigValidator checks them at startup
// (`php artisan app:check-config`). Values stay strings here so the
// validator sees what was written, not a silent cast.
return [
    'name' => env('SERVICE_NAME', '__NAME__'),
    'version' => env('APP_VERSION', 'dev'),

    'http' => [
        'max_body_bytes' => (string) env('HTTP_MAX_BODY_BYTES', '1048576'),
    ],

    // Read by the web server (Caddyfile), validated here.
    'shutdown_timeout' => (string) env('SHUTDOWN_TIMEOUT', '15s'),
    'drain_delay' => (string) env('DRAIN_DELAY', '0s'),

    'webhooks' => [
        // Enables POST /api/v1/webhooks/inbound. Unset: that route is 404.
        'inbound_secret' => (string) env('WEBHOOK_INBOUND_SECRET', ''),
        // Receives a signed `note.created` per new note. Unset: nothing is sent.
        'target_url' => (string) env('WEBHOOK_TARGET_URL', ''),
        'target_secret' => (string) env('WEBHOOK_TARGET_SECRET', ''),
    ],

    // Standard OpenTelemetry variables.
    'otel' => [
        'disabled' => env('OTEL_SDK_DISABLED', false),
        'endpoint' => (string) env('OTEL_EXPORTER_OTLP_ENDPOINT', ''),
        'timeout_ms' => (string) env('OTEL_EXPORTER_OTLP_TIMEOUT', '2000'),
        'sampler' => (string) env('OTEL_TRACES_SAMPLER', 'parentbased_always_on'),
        'sampler_arg' => (string) env('OTEL_TRACES_SAMPLER_ARG', '1.0'),
    ],
];
