<?php

namespace App\Jobs;

use App\Webhooks\DeliveryFailed;
use App\Webhooks\StandardWebhook;
use Illuminate\Contracts\Queue\ShouldQueue;
use Illuminate\Foundation\Queue\Queueable;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Facades\Log;
use OpenTelemetry\API\Trace\Propagation\TraceContextPropagator;
use OpenTelemetry\API\Trace\SpanInterface;
use OpenTelemetry\API\Trace\SpanKind;
use OpenTelemetry\API\Trace\StatusCode;
use OpenTelemetry\API\Trace\TracerInterface;
use Throwable;

/**
 * Delivers one signed webhook, run by `php artisan queue:work`.
 *
 * Semantics: at-least-once. The job row survives restarts (database queue);
 * a worker killed mid-delivery leaves the job reserved and it is retried
 * after `retry_after`. Receivers de-duplicate by webhook-id (= note id).
 * Never exactly-once.
 */
final class DeliverWebhook implements ShouldQueue
{
    use Queueable;

    /** Attempts in total, the first included. */
    public int $tries = 4;

    /**
     * Seconds one attempt may run before the worker kills it. Enforced only
     * with ext-pcntl; without it the HTTP timeout below bounds an attempt.
     */
    public int $timeout = 30;

    /** Seconds; the first retry waits up to BASE, then 2x, 4x (full jitter). */
    public const BACKOFF_BASE = 2;

    /**
     * @param  array<string, string>  $traceCarrier  W3C trace context of the request that created the note
     */
    public function __construct(
        public readonly string $webhookId,
        public readonly string $body,
        public readonly array $traceCarrier = [],
    ) {}

    /**
     * Full jitter: a random wait in [0, BASE * 2^n], so many failed
     * deliveries do not retry in lock-step against a receiver that is
     * recovering.
     *
     * @return list<int>
     */
    public function backoff(): array
    {
        $waits = [];
        for ($n = 0; $n < $this->tries - 1; $n++) {
            $waits[] = random_int(0, self::BACKOFF_BASE * (2 ** $n));
        }

        return $waits;
    }

    public function handle(TracerInterface $tracer): void
    {
        $url = (string) config('service.webhooks.target_url');
        $key = StandardWebhook::parseSecret((string) config('service.webhooks.target_secret'));

        $parent = TraceContextPropagator::getInstance()->extract($this->traceCarrier);
        $span = $tracer->spanBuilder('webhook.deliver')
            ->setParent($parent)
            ->setSpanKind(SpanKind::KIND_INTERNAL)
            ->setAttribute('webhook.id', $this->webhookId)
            ->setAttribute('webhook.attempt', $this->attempts())
            ->startSpan();
        $scope = $span->activate();
        try {
            // Signed at each attempt: the timestamp must be fresh for the
            // receiver's replay window; the id and body never change.
            $timestamp = time();
            try {
                $response = Http::timeout(5)
                    ->connectTimeout(3)
                    ->withHeaders([
                        StandardWebhook::HEADER_ID => $this->webhookId,
                        StandardWebhook::HEADER_TIMESTAMP => (string) $timestamp,
                        StandardWebhook::HEADER_SIGNATURE => StandardWebhook::sign($key, $this->webhookId, $timestamp, $this->body),
                    ])
                    ->withBody($this->body, 'application/json')
                    ->post($url);
            } catch (ConnectionException $e) {
                $this->recordFailure($span, $e);
                Log::warning('webhook delivery attempt failed', ['webhook_id' => $this->webhookId, 'attempt' => $this->attempts(), 'error' => $e->getMessage()]);
                throw $e; // retried with backoff until $tries
            }

            if ($response->successful()) {
                Log::info('webhook delivered', ['webhook_id' => $this->webhookId, 'status' => $response->status(), 'attempt' => $this->attempts()]);

                return;
            }
            $failure = new DeliveryFailed($response->status());
            $this->recordFailure($span, $failure);
            if ($failure->isRetryable()) {
                Log::warning('webhook delivery attempt failed', ['webhook_id' => $this->webhookId, 'attempt' => $this->attempts(), 'status' => $response->status()]);
                throw $failure;
            }
            Log::warning('webhook rejected by receiver', ['webhook_id' => $this->webhookId, 'status' => $response->status()]);
            $this->fail($failure); // final: another attempt would get the same 4xx
        } finally {
            $scope->detach();
            $span->end();
        }
    }

    /** Called once, when no attempt is left (or the job was failed as final). */
    public function failed(?Throwable $e): void
    {
        Log::error('webhook delivery failed', ['webhook_id' => $this->webhookId, 'error' => $e?->getMessage()]);
    }

    private function recordFailure(SpanInterface $span, Throwable $e): void
    {
        $span->recordException($e);
        $span->setStatus(StatusCode::STATUS_ERROR, $e->getMessage());
    }
}
