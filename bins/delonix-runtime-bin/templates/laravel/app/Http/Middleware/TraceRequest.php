<?php

namespace App\Http\Middleware;

use Closure;
use Illuminate\Http\Request;
use Illuminate\Routing\Route;
use OpenTelemetry\API\Trace\Propagation\TraceContextPropagator;
use OpenTelemetry\API\Trace\SpanKind;
use OpenTelemetry\API\Trace\StatusCode;
use OpenTelemetry\API\Trace\TracerInterface;
use Symfony\Component\HttpFoundation\Response;

/**
 * The SERVER span of a request, continuing the caller's W3C trace context.
 * Named by route template ("POST /api/v1/notes/{id}"), never by raw path.
 * Health probes are not traced.
 */
final class TraceRequest
{
    public function __construct(private readonly TracerInterface $tracer) {}

    public function handle(Request $request, Closure $next): Response
    {
        if (str_starts_with($request->path(), 'api/v1/health/')) {
            return $next($request);
        }
        $carrier = array_filter([
            'traceparent' => $request->headers->get('traceparent'),
            'tracestate' => $request->headers->get('tracestate'),
        ]);
        $span = $this->tracer->spanBuilder($request->method())
            ->setSpanKind(SpanKind::KIND_SERVER)
            ->setParent(TraceContextPropagator::getInstance()->extract($carrier))
            ->setAttribute('http.request.method', $request->method())
            ->setAttribute('url.path', '/'.ltrim($request->path(), '/'))
            ->startSpan();
        $scope = $span->activate();
        try {
            $response = $next($request);
            $route = $request->route();
            if ($route instanceof Route) {
                $template = '/'.ltrim($route->uri(), '/');
                $span->updateName($request->method().' '.$template);
                $span->setAttribute('http.route', $template);
            }
            $span->setAttribute('http.response.status_code', $response->getStatusCode());
            if ($response->getStatusCode() >= 500) {
                $span->setStatus(StatusCode::STATUS_ERROR);
            }

            return $response;
        } finally {
            $scope->detach();
            $span->end();
        }
    }
}
