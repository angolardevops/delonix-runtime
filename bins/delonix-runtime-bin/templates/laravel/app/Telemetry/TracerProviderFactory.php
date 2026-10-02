<?php

namespace App\Telemetry;

use OpenTelemetry\API\Common\Time\Clock;
use OpenTelemetry\API\Trace\NoopTracerProvider;
use OpenTelemetry\API\Trace\TracerProviderInterface;
use OpenTelemetry\Contrib\Otlp\OtlpHttpTransportFactory;
use OpenTelemetry\Contrib\Otlp\SpanExporter;
use OpenTelemetry\SDK\Common\Attribute\Attributes;
use OpenTelemetry\SDK\Resource\ResourceInfo;
use OpenTelemetry\SDK\Resource\ResourceInfoFactory;
use OpenTelemetry\SDK\Trace\Sampler\AlwaysOffSampler;
use OpenTelemetry\SDK\Trace\Sampler\AlwaysOnSampler;
use OpenTelemetry\SDK\Trace\Sampler\ParentBased;
use OpenTelemetry\SDK\Trace\Sampler\TraceIdRatioBasedSampler;
use OpenTelemetry\SDK\Trace\SamplerInterface;
use OpenTelemetry\SDK\Trace\SpanProcessor\BatchSpanProcessor;
use OpenTelemetry\SDK\Trace\SpanProcessorInterface;
use OpenTelemetry\SDK\Trace\TracerProvider;

/**
 * Builds the tracer provider from config('service'), i.e. from the standard
 * OTEL_* variables read by config/service.php (read through Laravel's config,
 * not getenv(), so `.env` and `config:cache` both work).
 *
 * - OTEL_SDK_DISABLED=true: a no-op provider, no trace ids anywhere.
 * - no OTEL_EXPORTER_OTLP_ENDPOINT: spans are recorded (logs still carry
 *   trace_id) but exported nowhere.
 * - an endpoint: OTLP over HTTP (JSON encoding, so no protobuf extension is
 *   needed), batched, flushed at the end of each request and each job.
 */
final class TracerProviderFactory
{
    /** @param array<string, mixed> $config config('service') */
    public static function make(array $config, ?SpanProcessorInterface $processor = null): TracerProviderInterface
    {
        $otel = $config['otel'];
        if (filter_var($otel['disabled'] ?? false, FILTER_VALIDATE_BOOL)) {
            return new NoopTracerProvider;
        }
        if ($processor === null && ($otel['endpoint'] ?? '') !== '') {
            $processor = new BatchSpanProcessor(self::exporter($otel), Clock::getDefault());
        }

        return new TracerProvider(
            $processor === null ? [] : [$processor],
            self::sampler((string) ($otel['sampler'] ?? ''), (string) ($otel['sampler_arg'] ?? '')),
            ResourceInfoFactory::emptyResource()->merge(ResourceInfo::create(Attributes::create([
                'service.name' => (string) $config['name'],
                'service.version' => (string) $config['version'],
                'deployment.environment.name' => (string) config('app.env'),
            ]))),
        );
    }

    /** @param array<string, mixed> $otel */
    private static function exporter(array $otel): SpanExporter
    {
        $endpoint = rtrim((string) $otel['endpoint'], '/').'/v1/traces';
        // Bounded: one attempt, no retries. A collector that does not answer
        // costs at most this per request, after the response was sent —
        // never a failed request.
        $timeout = max(0.1, ((int) $otel['timeout_ms']) / 1000);
        $transport = (new OtlpHttpTransportFactory)->create($endpoint, 'application/json', [], null, $timeout, 100, 0);

        return new SpanExporter($transport);
    }

    public static function sampler(string $name, string $arg): SamplerInterface
    {
        $ratio = is_numeric($arg) ? (float) $arg : 1.0;

        return match ($name) {
            'always_on' => new AlwaysOnSampler,
            'always_off' => new AlwaysOffSampler,
            'traceidratio' => new TraceIdRatioBasedSampler($ratio),
            'parentbased_always_off' => new ParentBased(new AlwaysOffSampler),
            'parentbased_traceidratio' => new ParentBased(new TraceIdRatioBasedSampler($ratio)),
            default => new ParentBased(new AlwaysOnSampler), // parentbased_always_on, the spec default
        };
    }
}
