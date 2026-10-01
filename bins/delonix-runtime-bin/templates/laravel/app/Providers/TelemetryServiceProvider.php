<?php

namespace App\Providers;

use App\Telemetry\HttpClientTracing;
use App\Telemetry\TracerProviderFactory;
use Illuminate\Queue\Events\JobFailed;
use Illuminate\Queue\Events\JobProcessed;
use Illuminate\Queue\Events\JobReleasedAfterException;
use Illuminate\Support\Facades\Event;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\ServiceProvider;
use OpenTelemetry\API\LoggerHolder;
use OpenTelemetry\API\Trace\TracerInterface;
use OpenTelemetry\API\Trace\TracerProviderInterface;
use OpenTelemetry\SDK\Trace\TracerProviderInterface as SdkTracerProvider;
use Psr\Log\LoggerInterface;

/**
 * OpenTelemetry wiring. PHP answers each request in a fresh application
 * (FrankenPHP classic mode, `artisan serve`), so spans are flushed when the
 * request terminates — after the response was sent — and after each queued
 * job in the long-running worker.
 */
class TelemetryServiceProvider extends ServiceProvider
{
    public function register(): void
    {
        $this->app->singleton(TracerProviderInterface::class, fn (): TracerProviderInterface => TracerProviderFactory::make((array) config('service')));
        $this->app->bind(TracerInterface::class, fn ($app): TracerInterface => $app->make(TracerProviderInterface::class)
            ->getTracer((string) config('service.name'), (string) config('service.version')));
    }

    public function boot(): void
    {
        // The SDK's own errors (a collector that is down) go to our JSON log.
        LoggerHolder::set($this->app->make(LoggerInterface::class));

        Http::globalMiddleware(HttpClientTracing::middleware(fn (): TracerInterface => $this->app->make(TracerInterface::class)));

        $flush = fn () => $this->flush();
        $this->app->terminating($flush);
        Event::listen(JobProcessed::class, $flush);
        Event::listen(JobFailed::class, $flush);
        Event::listen(JobReleasedAfterException::class, $flush);
    }

    private function flush(): void
    {
        if (! $this->app->resolved(TracerProviderInterface::class)) {
            return;
        }
        $provider = $this->app->make(TracerProviderInterface::class);
        // The binding is the SDK provider today; the check keeps a swapped-in
        // API-only provider from being asked to flush.
        if ($provider instanceof SdkTracerProvider) { // @phpstan-ignore instanceof.alwaysTrue
            $provider->forceFlush();
        }
    }
}
