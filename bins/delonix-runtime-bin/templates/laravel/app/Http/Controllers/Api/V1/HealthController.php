<?php

namespace App\Http\Controllers\Api\V1;

use Illuminate\Http\JsonResponse;
use Illuminate\Http\Request;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Log;
use Throwable;

/**
 * Liveness and readiness.
 *
 * - live: 200 whenever PHP answers.
 * - ready: 503 "draining" once the web server received SIGTERM (the Caddyfile
 *   sets X-Server-Draining from Caddy's {http.shutting_down}); 503 with
 *   "dependency": "database" when the database cannot be read or is not
 *   migrated ("starting"); 200 "ready" otherwise.
 *
 * Before the entrypoint has checked the configuration and migrated, nothing
 * listens at all: that is the "starting" window a probe sees as refused.
 */
final class HealthController
{
    public const DRAINING_HEADER = 'X-Server-Draining';

    public function live(): JsonResponse
    {
        return new JsonResponse(['status' => 'alive']);
    }

    public function ready(Request $request): JsonResponse
    {
        if ($request->headers->get(self::DRAINING_HEADER) === 'true') {
            return new JsonResponse(['status' => 'draining'], 503);
        }
        try {
            DB::connection()->getPdo();
        } catch (Throwable $e) {
            Log::warning('readiness: database unavailable', ['error' => $e->getMessage()]);

            return new JsonResponse(['status' => 'unavailable', 'dependency' => 'database'], 503);
        }
        try {
            DB::table('notes')->limit(1)->exists();
        } catch (Throwable) {
            return new JsonResponse(['status' => 'starting', 'dependency' => 'database'], 503);
        }

        return new JsonResponse(['status' => 'ready']);
    }
}
