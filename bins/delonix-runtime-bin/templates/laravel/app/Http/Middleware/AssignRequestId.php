<?php

namespace App\Http\Middleware;

use Closure;
use Illuminate\Http\Request;
use Illuminate\Support\Facades\Context;
use Symfony\Component\HttpFoundation\Response;

/**
 * Gives every request an id: the caller's X-Request-Id when it is a safe
 * token, a new random one otherwise. It is echoed in the response header,
 * put in every error body, and — through Laravel's Context — in every log
 * line of the request and of the jobs it queues.
 */
final class AssignRequestId
{
    public const HEADER = 'X-Request-Id';

    public function handle(Request $request, Closure $next): Response
    {
        $id = (string) $request->headers->get(self::HEADER, '');
        if (preg_match('/^[A-Za-z0-9._:-]{1,128}$/', $id) !== 1) {
            $id = bin2hex(random_bytes(16));
        }
        $request->attributes->set('request_id', $id);
        Context::add('request_id', $id);

        $response = $next($request);
        $response->headers->set(self::HEADER, $id);

        return $response;
    }
}
