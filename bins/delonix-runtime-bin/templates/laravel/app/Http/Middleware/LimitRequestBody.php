<?php

namespace App\Http\Middleware;

use App\Http\Errors\ApiError;
use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * Refuses a body over the limit with 413 body_too_large, before anything
 * parses it. `body.limit:api` uses HTTP_MAX_BODY_BYTES; `body.limit:webhook`
 * is the fixed 256 KiB of the inbound webhook.
 */
final class LimitRequestBody
{
    public const WEBHOOK_MAX_BYTES = 262_144;

    public function handle(Request $request, Closure $next, string $profile = 'api'): Response
    {
        $max = $profile === 'webhook'
            ? self::WEBHOOK_MAX_BYTES
            : (int) config('service.http.max_body_bytes');
        $declared = $request->headers->get('Content-Length');
        if (($declared !== null && (int) $declared > $max) || strlen($request->getContent()) > $max) {
            throw ApiError::bodyTooLarge();
        }

        return $next($request);
    }
}
