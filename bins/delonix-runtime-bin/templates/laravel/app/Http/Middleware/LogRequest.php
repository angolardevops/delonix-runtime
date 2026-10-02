<?php

namespace App\Http\Middleware;

use Closure;
use Illuminate\Http\Request;
use Illuminate\Routing\Route;
use Illuminate\Support\Facades\Log;
use Symfony\Component\HttpFoundation\Response;

/**
 * One access-log line per request: method, route template, status and
 * duration. Never the query string, headers or body. Health probes at debug.
 */
final class LogRequest
{
    public function handle(Request $request, Closure $next): Response
    {
        $started = hrtime(true);
        $response = $next($request);
        $route = $request->route();
        $fields = [
            'method' => $request->method(),
            'route' => $route instanceof Route ? '/'.ltrim($route->uri(), '/') : null,
            'status' => $response->getStatusCode(),
            'duration_ms' => round((hrtime(true) - $started) / 1e6, 2),
        ];
        if (str_starts_with($request->path(), 'api/v1/health/')) {
            Log::debug('request', $fields);
        } else {
            Log::info('request', $fields);
        }

        return $response;
    }
}
