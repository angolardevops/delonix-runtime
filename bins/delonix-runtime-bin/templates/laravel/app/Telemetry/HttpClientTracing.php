<?php

namespace App\Telemetry;

use Closure;
use GuzzleHttp\Promise\PromiseInterface;
use OpenTelemetry\API\Trace\Propagation\TraceContextPropagator;
use OpenTelemetry\API\Trace\SpanKind;
use OpenTelemetry\API\Trace\StatusCode;
use OpenTelemetry\API\Trace\TracerInterface;
use OpenTelemetry\Context\Context;
use Psr\Http\Message\RequestInterface;
use Psr\Http\Message\ResponseInterface;
use Throwable;

/**
 * Guzzle middleware for Laravel's HTTP client: a CLIENT span per outgoing
 * request, and the W3C `traceparent` header injected so the receiver can
 * join the same trace. Registered globally in TelemetryServiceProvider.
 */
final class HttpClientTracing
{
    /** @param Closure(): TracerInterface $tracer resolved per request, so tests can swap it */
    public static function middleware(Closure $tracer): Closure
    {
        return static fn (callable $handler): Closure => static function (RequestInterface $request, array $options) use ($handler, $tracer): PromiseInterface {
            $span = $tracer()->spanBuilder($request->getMethod())
                ->setSpanKind(SpanKind::KIND_CLIENT)
                ->setAttribute('http.request.method', $request->getMethod())
                ->setAttribute('server.address', $request->getUri()->getHost())
                ->startSpan();
            $context = $span->storeInContext(Context::getCurrent());
            $headers = [];
            TraceContextPropagator::getInstance()->inject($headers, null, $context);
            foreach ($headers as $name => $value) {
                $request = $request->withHeader($name, $value);
            }

            return $handler($request, $options)->then(
                static function (ResponseInterface $response) use ($span): ResponseInterface {
                    $span->setAttribute('http.response.status_code', $response->getStatusCode());
                    if ($response->getStatusCode() >= 400) {
                        $span->setStatus(StatusCode::STATUS_ERROR);
                    }
                    $span->end();

                    return $response;
                },
                static function (mixed $reason) use ($span): never {
                    if ($reason instanceof Throwable) {
                        $span->recordException($reason);
                    }
                    $span->setStatus(StatusCode::STATUS_ERROR);
                    $span->end();
                    throw $reason instanceof Throwable ? $reason : new \RuntimeException('HTTP request failed');
                },
            );
        };
    }
}
