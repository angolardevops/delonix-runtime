<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    public function up(): void
    {
        // De-duplication of inbound webhooks by `webhook-id`. In the database
        // (not the cache) so it survives restarts and is shared by replicas.
        Schema::create('webhook_receipts', function (Blueprint $table): void {
            $table->string('webhook_id', 255)->primary();
            $table->timestamp('received_at', 6)->index();
        });
    }

    public function down(): void
    {
        Schema::dropIfExists('webhook_receipts');
    }
};
