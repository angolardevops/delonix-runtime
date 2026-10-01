<?php

// Nothing in the service caches: the in-process store keeps the database
// free of a cache table. Webhook de-duplication is in the database, not here.
return [
    'default' => env('CACHE_STORE', 'array'),
];
