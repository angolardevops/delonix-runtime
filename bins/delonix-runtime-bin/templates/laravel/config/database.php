<?php

// Merged over Laravel's own config/database.php: only the SQLite connection
// is redefined, tuned for a web process and a queue worker sharing one file.
return [
    'default' => env('DB_CONNECTION', 'sqlite'),

    'connections' => [
        'sqlite' => [
            'driver' => 'sqlite',
            'url' => env('DB_URL'),
            'database' => env('DB_DATABASE', database_path('database.sqlite')),
            'prefix' => '',
            'foreign_key_constraints' => true,
            // Wait for a lock instead of failing at once when the worker writes.
            'busy_timeout' => 5000,
            // Readers do not block the writer.
            'journal_mode' => 'wal',
            'synchronous' => 'normal',
            'transaction_mode' => 'DEFERRED',
        ],
    ],
];
