<?php

namespace Tests\Feature;

use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Support\Facades\Schema;
use Tests\TestCase;

final class HealthTest extends TestCase
{
    use RefreshDatabase;

    public function test_live_answers_while_the_process_runs(): void
    {
        $this->getJson('/api/v1/health/live')->assertOk()->assertExactJson(['status' => 'alive']);
    }

    public function test_ready_when_the_database_is_migrated(): void
    {
        $this->getJson('/api/v1/health/ready')->assertOk()->assertExactJson(['status' => 'ready']);
    }

    public function test_draining_once_the_server_says_it_is_shutting_down(): void
    {
        // The Caddyfile sets this header from Caddy's {http.shutting_down}.
        $this->getJson('/api/v1/health/ready', ['X-Server-Draining' => 'true'])
            ->assertStatus(503)->assertExactJson(['status' => 'draining']);
        $this->getJson('/api/v1/health/ready', ['X-Server-Draining' => 'false'])->assertOk();
    }

    public function test_starting_while_the_schema_is_not_migrated(): void
    {
        Schema::drop('notes');

        $this->getJson('/api/v1/health/ready')
            ->assertStatus(503)->assertExactJson(['status' => 'starting', 'dependency' => 'database']);
    }

    public function test_unavailable_when_the_database_cannot_be_opened(): void
    {
        // A second connection, so the test database of the other tests stays intact.
        config([
            'database.connections.broken' => ['driver' => 'sqlite', 'database' => '/nonexistent-dir/db.sqlite'],
            'database.default' => 'broken',
        ]);
        try {
            $this->getJson('/api/v1/health/ready')
                ->assertStatus(503)->assertExactJson(['status' => 'unavailable', 'dependency' => 'database']);
            // Liveness does not depend on the database.
            $this->getJson('/api/v1/health/live')->assertOk();
        } finally {
            config(['database.default' => 'sqlite']);
        }
    }
}
