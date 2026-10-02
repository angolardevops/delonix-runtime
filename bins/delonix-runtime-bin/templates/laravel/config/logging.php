<?php

use App\Logging\ConfigureJsonLogging;
use Monolog\Handler\StreamHandler;

// Merged over Laravel's own config/logging.php: only what differs is here.
return [
    'default' => env('LOG_CHANNEL', 'json'),

    'channels' => [
        // One JSON line per event on the process output (12-factor): the
        // container runtime collects it. See app/Logging.
        'json' => [
            'driver' => 'monolog',
            'level' => env('LOG_LEVEL', 'info'),
            'handler' => StreamHandler::class,
            'with' => ['stream' => env('LOG_STREAM', 'php://stdout')],
            'tap' => [ConfigureJsonLogging::class],
        ],
    ],
];
