<?php

// Merged over Laravel's own config/queue.php (which defines the `sync`,
// `database` and `redis` connections). QUEUE_CONNECTION picks one:
//
//   database  — the jobs table; survives a restart with no broker to run
//               (the default for a bare `composer serve`).
//   rabbitmq  — the `rabbitmq` connection below, delivered by
//               `php artisan queue:work`. compose.yaml sets
//               QUEUE_CONNECTION=rabbitmq and runs the broker + a worker.
//   sync      — run inline, in the request (the test suite forces this).
//
// Outbound webhooks (App\Jobs\DeliverWebhook) ride whichever connection is
// chosen; the publisher (App\Webhooks\QueuedNotePublisher) does not care.
//
// The `rabbitmq` driver comes from vladimir-yuldashev/laravel-queue-rabbitmq
// (composer.json); its service provider auto-registers the connector.
return [
    'default' => env('QUEUE_CONNECTION', 'database'),

    'connections' => [
        'rabbitmq' => [
            'driver' => 'rabbitmq',
            'hosts' => [
                [
                    'host' => env('RABBITMQ_HOST', '127.0.0.1'),
                    'port' => (int) env('RABBITMQ_PORT', 5672),
                    'user' => env('RABBITMQ_USER', 'guest'),
                    'password' => env('RABBITMQ_PASSWORD', 'guest'),
                    'vhost' => env('RABBITMQ_VHOST', '/'),
                ],
            ],
            'queue' => env('RABBITMQ_QUEUE', 'default'),
            'options' => [
                'heartbeat' => 30,
                'connection_timeout' => 3.0,
                'read_timeout' => 3.0,
                'write_timeout' => 3.0,
                'channel_rpc_timeout' => 3.0,
            ],
            // Never publish an event for a row that was rolled back; the job
            // also sets afterCommit() itself.
            'after_commit' => true,
            'lazy' => true,
            'network_protocol' => 'tcp',
        ],
    ],
];
