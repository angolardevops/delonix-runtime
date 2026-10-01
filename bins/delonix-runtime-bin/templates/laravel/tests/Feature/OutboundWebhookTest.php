<?php

namespace Tests\Feature;

use App\Jobs\DeliverWebhook;
use App\Webhooks\DeliveryFailed;
use App\Webhooks\StandardWebhook;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Facades\Queue;
use OpenTelemetry\API\Trace\TracerInterface;
use Tests\TestCase;

final class OutboundWebhookTest extends TestCase
{
    use RefreshDatabase;

    private const URL = 'https://receiver.test/hooks';

    protected function setUp(): void
    {
        parent::setUp();
        config(['service.webhooks.target_url' => self::URL, 'service.webhooks.target_secret' => self::WEBHOOK_SECRET]);
    }

    public function test_nothing_is_queued_without_a_target(): void
    {
        config(['service.webhooks.target_url' => '']);
        Queue::fake();

        $this->postJson('/api/v1/notes', ['title' => 't'])->assertCreated();

        Queue::assertNothingPushed();
    }

    public function test_a_created_note_queues_one_delivery_keyed_by_the_note_id(): void
    {
        Queue::fake();

        $id = $this->postJson('/api/v1/notes', ['title' => 't'])->assertCreated()->json('id');

        Queue::assertPushed(DeliverWebhook::class, 1);
        Queue::assertPushed(fn (DeliverWebhook $job): bool => $job->webhookId === $id
            && json_decode($job->body, true)['type'] === 'note.created'
            && json_decode($job->body, true)['data']['id'] === $id);
    }

    public function test_the_delivery_is_signed_over_the_exact_body(): void
    {
        Http::fake([self::URL => Http::response('', 204)]);

        $this->runJob(new DeliverWebhook('note-1', '{"type":"note.created"}'));

        Http::assertSentCount(1);
        Http::assertSent(function (Request $request): bool {
            StandardWebhook::verify(
                StandardWebhook::parseSecret(self::WEBHOOK_SECRET),
                $request->header('webhook-id')[0],
                $request->header('webhook-timestamp')[0],
                $request->header('webhook-signature')[0],
                $request->body(),
                time(),
            );

            return $request->url() === self::URL && $request->header('webhook-id')[0] === 'note-1';
        });
    }

    public function test_5xx_429_and_network_errors_are_retried(): void
    {
        foreach ([500, 503, 429] as $status) {
            Http::fake([self::URL => Http::response('', $status)]);
            try {
                $this->runJob(new DeliverWebhook('note-1', '{}'));
                $this->fail("status {$status} must throw so the queue retries");
            } catch (DeliveryFailed $e) {
                $this->assertTrue($e->isRetryable());
            }
        }

        Http::fake(fn () => throw new ConnectionException('connection refused'));
        $this->expectException(ConnectionException::class);
        $this->runJob(new DeliverWebhook('note-1', '{}'));
    }

    public function test_other_4xx_are_final(): void
    {
        Http::fake([self::URL => Http::response('', 400)]);
        $job = (new DeliverWebhook('note-1', '{}'))->withFakeQueueInteractions();

        $this->runJob($job);

        $job->assertFailed();
        $job->assertNotReleased();
    }

    public function test_backoff_is_full_jitter_bounded_and_one_shorter_than_the_attempts(): void
    {
        $job = new DeliverWebhook('note-1', '{}');
        $this->assertSame(4, $job->tries);
        $seen = [];
        for ($i = 0; $i < 200; $i++) {
            $waits = $job->backoff();
            $this->assertCount(3, $waits);
            foreach ($waits as $n => $wait) {
                $this->assertGreaterThanOrEqual(0, $wait);
                $this->assertLessThanOrEqual(DeliverWebhook::BACKOFF_BASE * 2 ** $n, $wait);
                $seen[$n][$wait] = true;
            }
        }
        // Jitter: the last wait takes many values, it is not a fixed schedule.
        $this->assertGreaterThan(3, count($seen[2]));
    }

    private function runJob(DeliverWebhook $job): void
    {
        $job->handle($this->app->make(TracerInterface::class));
    }
}
