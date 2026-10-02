<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    public function up(): void
    {
        Schema::create('notes', function (Blueprint $table): void {
            $table->string('id', 32)->primary();
            $table->string('title', 200);
            $table->text('body');
            // Microseconds, so "newest first" is stable within one second.
            $table->timestamp('created_at', 6)->index();
        });
    }

    public function down(): void
    {
        Schema::dropIfExists('notes');
    }
};
