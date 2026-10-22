<?php

// Merged over Laravel's own config/cache.php (which defines the `redis` store
// and the `array`, `database`, `file` stores). CACHE_STORE picks one:
//
//   array  — in-process, nothing to run (the default for a bare `composer serve`)
//   redis  — read-through Redis; compose.yaml sets CACHE_STORE=redis and
//            REDIS_URL to the `redis` service. The redis connection is the one
//            in config/database.php (framework default, reads REDIS_URL).
//
// Webhook de-duplication is in the database, not here.
return [
    'default' => env('CACHE_STORE', 'array'),
    'prefix' => env('CACHE_PREFIX', '__NAME___cache'),
];
