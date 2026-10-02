<?php

namespace Tests\Feature;

use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Support\Facades\Schema;
use Tests\TestCase;

final class NotesApiTest extends TestCase
{
    use RefreshDatabase;

    public function test_note_lifecycle(): void
    {
        $created = $this->postJson('/api/v1/notes', ['title' => '  hello  ', 'body' => 'world'])
            ->assertCreated()
            ->assertJsonPath('title', 'hello')
            ->assertJsonPath('body', 'world')
            ->assertJsonStructure(['id', 'title', 'body', 'created_at']);
        $id = $created->json('id');
        $created->assertHeader('Location', '/api/v1/notes/'.$id);
        $this->assertMatchesRegularExpression('/^[0-9a-f]{32}$/', $id);

        $this->getJson('/api/v1/notes/'.$id)->assertOk()->assertExactJson($created->json());
        $this->assertDatabaseHas('notes', ['id' => $id, 'title' => 'hello']);
    }

    public function test_list_is_paginated_newest_first(): void
    {
        $ids = [];
        foreach (['a', 'b', 'c'] as $title) {
            $ids[] = $this->postJson('/api/v1/notes', ['title' => $title])->assertCreated()->json('id');
        }

        $this->getJson('/api/v1/notes')
            ->assertOk()
            ->assertJsonPath('total', 3)->assertJsonPath('offset', 0)->assertJsonPath('limit', 20)
            ->assertJsonPath('items.0.id', $ids[2])->assertJsonPath('items.2.id', $ids[0]);

        $this->getJson('/api/v1/notes?offset=1&limit=1')
            ->assertOk()
            ->assertJsonCount(1, 'items')
            ->assertJsonPath('items.0.id', $ids[1])
            ->assertJsonPath('total', 3)->assertJsonPath('offset', 1)->assertJsonPath('limit', 1);
    }

    public function test_every_error_has_the_one_shape(): void
    {
        $cases = [
            'empty title' => [$this->postJson('/api/v1/notes', ['title' => '   ']), 422, 'validation_failed', ['title']],
            'title too long' => [$this->postJson('/api/v1/notes', ['title' => str_repeat('x', 201)]), 422, 'validation_failed', ['title']],
            'body too long' => [$this->postJson('/api/v1/notes', ['title' => 't', 'body' => str_repeat('x', 10_001)]), 422, 'validation_failed', ['body']],
            'unknown field' => [$this->postJson('/api/v1/notes', ['title' => 't', 'titel' => 'typo']), 422, 'validation_failed', ['titel']],
            'limit out of range' => [$this->getJson('/api/v1/notes?limit=101'), 422, 'validation_failed', ['limit']],
            'offset not a number' => [$this->getJson('/api/v1/notes?offset=abc'), 422, 'validation_failed', ['offset']],
            'malformed json' => [$this->call('POST', '/api/v1/notes', server: ['CONTENT_TYPE' => 'application/json'], content: '{"title":'), 400, 'malformed_json', null],
            'json that is not an object' => [$this->call('POST', '/api/v1/notes', server: ['CONTENT_TYPE' => 'application/json'], content: '["title"]'), 400, 'malformed_json', null],
            'missing note' => [$this->getJson('/api/v1/notes/does-not-exist'), 404, 'not_found', null],
            'unknown route' => [$this->getJson('/api/v1/nope'), 404, 'not_found', null],
            'wrong method' => [$this->deleteJson('/api/v1/notes'), 405, 'method_not_allowed', null],
        ];

        foreach ($cases as $name => [$response, $status, $code, $fields]) {
            $response->assertStatus($status);
            $error = $response->json('error');
            $this->assertSame($code, $error['code'], $name);
            $this->assertIsString($error['message'], $name);
            $this->assertNotSame('', $error['request_id'], $name);
            $this->assertSame($error['request_id'], $response->headers->get('X-Request-Id'), $name);
            $this->assertSame($fields, isset($error['fields']) ? array_keys($error['fields']) : null, $name);
            $this->assertSame(['error'], array_keys($response->json()), $name);
        }
    }

    public function test_a_body_over_the_limit_is_413(): void
    {
        config(['service.http.max_body_bytes' => '64']);

        $this->postJson('/api/v1/notes', ['title' => 't', 'body' => str_repeat('x', 100)])
            ->assertStatus(413)
            ->assertJsonPath('error.code', 'body_too_large');
        $this->assertDatabaseCount('notes', 0);
    }

    public function test_an_unexpected_failure_is_500_without_internals(): void
    {
        // Break the storage under the use case: the client must learn nothing.
        Schema::drop('notes');

        $response = $this->postJson('/api/v1/notes', ['title' => 't'])->assertStatus(500);

        $this->assertSame('internal', $response->json('error.code'));
        $this->assertSame('internal error', $response->json('error.message'));
        $this->assertStringNotContainsStringIgnoringCase('sql', (string) $response->getContent());
        $this->assertStringNotContainsString('trace', (string) $response->getContent());
        $this->assertStringNotContainsString('.php', (string) $response->getContent());
    }

    public function test_the_callers_request_id_is_kept_when_safe_and_replaced_when_not(): void
    {
        $this->getJson('/api/v1/notes', ['X-Request-Id' => 'abc-123'])->assertHeader('X-Request-Id', 'abc-123');

        $unsafe = $this->getJson('/api/v1/notes', ['X-Request-Id' => "bad id\twith spaces"])->headers->get('X-Request-Id');
        $this->assertMatchesRegularExpression('/^[0-9a-f]{32}$/', (string) $unsafe);
    }
}
