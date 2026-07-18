<?php

namespace Tests\Feature;

use Tests\TestCase;

class HealthTest extends TestCase
{
    public function test_health_live_returns_alive(): void
    {
        $this->getJson('/api/v1/health/live')
            ->assertOk()
            ->assertJson(['status' => 'alive']);
    }

    public function test_health_ready_returns_ready(): void
    {
        $this->getJson('/api/v1/health/ready')
            ->assertOk()
            ->assertJson(['status' => 'ready']);
    }
}
