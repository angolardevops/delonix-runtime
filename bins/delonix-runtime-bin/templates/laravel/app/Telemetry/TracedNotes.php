<?php

namespace App\Telemetry;

use App\Notes\InvalidNote;
use App\Notes\Note;
use App\Notes\NoteNotFound;
use App\Notes\NotePage;
use App\Notes\Notes;
use OpenTelemetry\API\Trace\StatusCode;
use OpenTelemetry\API\Trace\TracerInterface;
use Throwable;

/**
 * Decorates the use cases with one span each, so App\Notes stays free of
 * telemetry imports. Expected domain outcomes (invalid input, not found) are
 * recorded as events, not as span errors.
 */
final class TracedNotes implements Notes
{
    public function __construct(
        private readonly Notes $next,
        private readonly TracerInterface $tracer,
    ) {}

    public function create(string $title, ?string $body): Note
    {
        return $this->traced('notes.create', fn (): Note => $this->next->create($title, $body));
    }

    public function get(string $id): Note
    {
        return $this->traced('notes.get', fn (): Note => $this->next->get($id));
    }

    public function list(int $offset, int $limit): NotePage
    {
        return $this->traced('notes.list', fn (): NotePage => $this->next->list($offset, $limit));
    }

    /**
     * @template T
     *
     * @param  callable(): T  $call
     * @return T
     */
    private function traced(string $name, callable $call): mixed
    {
        $span = $this->tracer->spanBuilder($name)->startSpan();
        $scope = $span->activate();
        try {
            return $call();
        } catch (InvalidNote|NoteNotFound $e) {
            $span->addEvent($e->getMessage());
            throw $e;
        } catch (Throwable $e) {
            $span->recordException($e);
            $span->setStatus(StatusCode::STATUS_ERROR, $e->getMessage());
            throw $e;
        } finally {
            $scope->detach();
            $span->end();
        }
    }
}
