<?php

// Laravel's `database` queue (the jobs table): outbound webhooks survive a
// restart and are delivered by `php artisan queue:work`.
return [
    'default' => env('QUEUE_CONNECTION', 'database'),
];
