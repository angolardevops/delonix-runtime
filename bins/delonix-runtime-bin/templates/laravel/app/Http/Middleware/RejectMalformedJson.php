<?php

namespace App\Http\Middleware;

use App\Http\Errors\ApiError;
use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * Laravel reads an unparsable JSON body as "no input", which would surface
 * as a misleading 422. This answers 400 malformed_json instead, for any
 * non-empty body that is not a JSON object.
 */
final class RejectMalformedJson
{
    public function handle(Request $request, Closure $next): Response
    {
        $body = $request->getContent();
        if ($body !== '' && ! in_array($request->method(), ['GET', 'HEAD'], true)) {
            try {
                $decoded = json_decode($body, false, 64, JSON_THROW_ON_ERROR);
            } catch (\JsonException) {
                throw ApiError::malformedJson('request body is not valid JSON');
            }
            if (! $decoded instanceof \stdClass) {
                throw ApiError::malformedJson('request body must be a JSON object');
            }
        }

        return $next($request);
    }
}
