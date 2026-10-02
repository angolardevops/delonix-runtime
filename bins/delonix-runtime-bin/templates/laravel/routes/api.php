<?php

use App\Http\Controllers\Api\V1\HealthController;
use App\Http\Controllers\Api\V1\InboundWebhookController;
use App\Http\Controllers\Api\V1\NoteController;
use Illuminate\Support\Facades\Route;

// Mounted under /api by bootstrap/app.php. Every route here is in
// api/openapi.yaml; tests/Feature/OpenApiContractTest fails when they drift.
Route::prefix('v1')->group(function (): void {
    Route::get('health/live', [HealthController::class, 'live']);
    Route::get('health/ready', [HealthController::class, 'ready']);

    Route::middleware(['body.limit:api', 'json.object'])->group(function (): void {
        Route::post('notes', [NoteController::class, 'store']);
        Route::get('notes', [NoteController::class, 'index']);
        Route::get('notes/{id}', [NoteController::class, 'show']);
    });

    // Verified over the raw bytes, so no JSON middleware in front of it.
    Route::post('webhooks/inbound', InboundWebhookController::class)->middleware('body.limit:webhook');
});
