<?php

use Illuminate\Support\Facades\Route;

// Health probes — the Delonixfile HEALTHCHECK and `--up` hit /api/v1/health/live.
// Registered on the stateless `api` group (no session/cookie layer) so they
// answer 200 regardless of app state.
Route::get('/v1/health/live', fn () => response()->json(['status' => 'alive']));
Route::get('/v1/health/ready', fn () => response()->json(['status' => 'ready']));
