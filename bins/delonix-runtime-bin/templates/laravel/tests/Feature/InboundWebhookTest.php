<?php

namespace Tests\Feature;

use App\Webhooks\StandardWebhook;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Testing\TestResponse;
use Symfony\Component\HttpFoundation\Response;
use Tests\TestCase;

final class InboundWebhookTest extends TestCase
{
    use RefreshDatabase;

    protected function setUp(): void
    {
        parent::setUp();
        config(['service.webhooks.inbound_secret' => self::WEBHOOK_SECRET]);
    }

    /** @return TestResponse<Response> */
    private function deliver(string $id, string $body, ?int $timestamp = null, ?string $signature = null): TestResponse
    {
        $timestamp ??= time();
        $signature ??= StandardWebhook::sign(StandardWebhook::parseSecret(self::WEBHOOK_SECRET), $id, $timestamp, $body);

        return $this->call('POST', '/api/v1/webhooks/inbound', server: [
            'CONTENT_TYPE' => 'application/json',
            'HTTP_WEBHOOK_ID' => $id,
            'HTTP_WEBHOOK_TIMESTAMP' => (string) $timestamp,
            'HTTP_WEBHOOK_SIGNATURE' => $signature,
        ], content: $body);
    }

    public function test_the_endpoint_is_off_without_a_secret(): void
    {
        config(['service.webhooks.inbound_secret' => '']);

        $this->deliver('msg_1', '{"type":"ping"}')->assertStatus(404)->assertJsonPath('error.code', 'not_found');
    }

    public function test_a_signed_ping_is_accepted(): void
    {
        $this->deliver('msg_1', '{"type":"ping"}')->assertNoContent();
        $this->assertDatabaseHas('webhook_receipts', ['webhook_id' => 'msg_1']);
    }

    public function test_note_create_runs_the_use_case_once_however_often_it_is_delivered(): void
    {
        // Odd spacing on purpose: the signature is over these exact bytes.
        $body = '{ "type":"note.create",  "data":{"title":"from webhook","body":"b"} }';

        $this->deliver('msg_2', $body)->assertNoContent();
        $this->deliver('msg_2', $body)->assertNoContent(); // the sender's retry
        $this->deliver('msg_2', $body, time() + 5)->assertNoContent();

        $this->assertDatabaseCount('notes', 1);
        $this->assertDatabaseHas('notes', ['title' => 'from webhook']);
    }

    public function test_bad_deliveries_are_refused_and_leave_nothing(): void
    {
        $this->deliver('msg_3', '{"type":"ping"}', signature: 'v1,'.base64_encode('nope'))
            ->assertStatus(401)->assertJsonPath('error.code', 'invalid_signature');
        $this->deliver('msg_3', '{"type":"ping"}', time() - 600)
            ->assertStatus(401)->assertJsonPath('error.code', 'invalid_signature');
        $this->call('POST', '/api/v1/webhooks/inbound', content: '{"type":"ping"}')
            ->assertStatus(400)->assertJsonPath('error.code', 'missing_signature');
        $this->deliver('msg_3', 'not json')->assertStatus(400)->assertJsonPath('error.code', 'malformed_json');
        $this->deliver('msg_3', '{"type":"other"}')->assertStatus(422)->assertJsonPath('error.code', 'unknown_event');
        $this->deliver('msg_3', '{"type":"ping"}'.str_repeat(' ', 262_144))
            ->assertStatus(413)->assertJsonPath('error.code', 'body_too_large');

        $this->assertDatabaseCount('webhook_receipts', 0);
    }

    public function test_a_delivery_that_failed_validation_can_be_retried_with_the_same_id(): void
    {
        $this->deliver('msg_4', '{"type":"note.create","data":{"title":""}}')
            ->assertStatus(422)->assertJsonPath('error.fields.title', 'is required');
        $this->assertDatabaseCount('webhook_receipts', 0);

        $this->deliver('msg_4', '{"type":"note.create","data":{"title":"fixed"}}')->assertNoContent();
        $this->assertDatabaseCount('notes', 1);
    }
}
