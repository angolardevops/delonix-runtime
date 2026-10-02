<?php

namespace Tests\Feature;

use App\Logging\ConfigureJsonLogging;
use App\Telemetry\TracerProviderFactory;
use App\Webhooks\StandardWebhook;
use ArrayObject;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Facades\Log;
use Monolog\Handler\TestHandler;
use Monolog\Logger;
use OpenTelemetry\API\Trace\TracerProviderInterface;
use OpenTelemetry\SDK\Trace\SpanDataInterface;
use OpenTelemetry\SDK\Trace\SpanExporter\InMemoryExporter;
use OpenTelemetry\SDK\Trace\SpanProcessor\SimpleSpanProcessor;
use Tests\TestCase;

/**
 * One request that creates a note must leave ONE trace across the incoming
 * request, the use case, the queued job, the outbound webhook call and the
 * log lines — that is what makes a failure findable from any of them.
 *
 * The job really goes through the `database` queue and is run by
 * `queue:work`, as in production: the trace context survives the queue.
 */
final class OneTraceTest extends TestCase
{
    use RefreshDatabase;

    public function test_one_trace_across_request_use_case_queue_outbound_call_and_logs(): void
    {
        /** @var ArrayObject<int, SpanDataInterface> $spans */
        $spans = new ArrayObject;
        $this->app->instance(TracerProviderInterface::class, TracerProviderFactory::make(
            (array) config('service'),
            new SimpleSpanProcessor(new InMemoryExporter($spans)),
        ));
        config([
            'queue.default' => 'database',
            'service.webhooks.target_url' => 'https://receiver.test/hooks',
            'service.webhooks.target_secret' => self::WEBHOOK_SECRET,
            'logging.default' => 'capture',
            'logging.channels.capture' => ['driver' => 'monolog', 'handler' => TestHandler::class, 'tap' => [ConfigureJsonLogging::class]],
        ]);
        $received = [];
        Http::fake(function (Request $request) use (&$received) {
            $received[] = $request;

            return Http::response('', 204);
        });

        $this->postJson('/api/v1/notes', ['title' => 'traced'])->assertCreated();
        $this->assertDatabaseCount('jobs', 1);
        $this->assertSame([], $received, 'the request must not wait for the delivery');
        $this->artisan('queue:work', ['--once' => true])->assertSuccessful();
        $this->assertDatabaseCount('jobs', 0);

        // Spans: all in the trace of the server span.
        $traceOf = [];
        foreach ($spans as $span) {
            $traceOf[$span->getName()] = $span->getTraceId();
        }
        $this->assertArrayHasKey('POST /api/v1/notes', $traceOf, 'no server span; spans: '.implode(', ', array_keys($traceOf)));
        $trace = $traceOf['POST /api/v1/notes'];
        foreach (['notes.create', 'webhook.deliver', 'POST'] as $name) {
            $this->assertSame($trace, $traceOf[$name] ?? null, "span {$name} is not in the request's trace");
        }

        // The outbound call: signed, and carrying the same trace id.
        /** @var list<Request> $received */
        $this->assertCount(1, $received);
        $this->assertStringContainsString($trace, $received[0]->header('traceparent')[0] ?? '');
        StandardWebhook::verify(
            StandardWebhook::parseSecret(self::WEBHOOK_SECRET),
            $received[0]->header('webhook-id')[0],
            $received[0]->header('webhook-timestamp')[0],
            $received[0]->header('webhook-signature')[0],
            $received[0]->body(),
            time(),
        );

        // Log lines: the access log and the delivery share the trace id and the request id.
        /** @var \Illuminate\Log\Logger $channel */
        $channel = Log::channel('capture');
        /** @var Logger $monolog */
        $monolog = $channel->getLogger();
        $handler = $monolog->getHandlers()[0];
        $this->assertInstanceOf(TestHandler::class, $handler);
        $lines = [];
        foreach ($handler->getRecords() as $record) {
            $line = json_decode((string) $handler->getFormatter()->format($record), true);
            if (($line['trace_id'] ?? null) === $trace) {
                $lines[$line['msg']] = $line;
            }
        }
        $this->assertArrayHasKey('request', $lines, 'no access-log line with the trace id');
        $this->assertArrayHasKey('webhook delivered', $lines, 'no delivery log line with the trace id');
        $this->assertNotEmpty($lines['request']['request_id']);
        $this->assertSame($lines['request']['request_id'], $lines['webhook delivered']['request_id']);
    }
}
