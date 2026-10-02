<?php

namespace App\Providers;

use App\Notes\NotePublisher;
use App\Notes\NoteRepository;
use App\Notes\Notes;
use App\Notes\NoteService;
use App\Notes\NullPublisher;
use App\Persistence\EloquentNoteRepository;
use App\Telemetry\TracedNotes;
use App\Webhooks\QueuedNotePublisher;
use Illuminate\Http\Resources\Json\JsonResource;
use Illuminate\Support\ServiceProvider;
use OpenTelemetry\API\Trace\TracerInterface;

/**
 * Composition root: the only place that chooses adapters for the ports.
 */
class AppServiceProvider extends ServiceProvider
{
    public function register(): void
    {
        $this->app->bind(NoteRepository::class, EloquentNoteRepository::class);
        $this->app->bind(NotePublisher::class, fn (): NotePublisher => (string) config('service.webhooks.target_url') !== ''
            ? new QueuedNotePublisher
            : new NullPublisher);
        $this->app->bind(Notes::class, fn ($app): Notes => new TracedNotes(
            new NoteService($app->make(NoteRepository::class), $app->make(NotePublisher::class)),
            $app->make(TracerInterface::class),
        ));
    }

    public function boot(): void
    {
        // Responses are the resource itself, not {"data": ...}.
        JsonResource::withoutWrapping();
    }
}
