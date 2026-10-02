<?php

namespace Tests\Unit;

use App\Notes\InvalidNote;
use App\Notes\Note;
use App\Notes\NoteNotFound;
use App\Notes\NotePage;
use App\Notes\NotePublisher;
use App\Notes\NoteRepository;
use App\Notes\NoteService;
use PHPUnit\Framework\TestCase;

/** The use cases against in-memory ports: no framework, no database. */
final class NoteServiceTest extends TestCase
{
    /** @var list<Note> */
    private array $published = [];

    private NoteService $service;

    protected function setUp(): void
    {
        $repository = new class implements NoteRepository
        {
            /** @var array<string, Note> */
            public array $rows = [];

            public function save(Note $note): void
            {
                $this->rows[$note->id] = $note;
            }

            public function find(string $id): ?Note
            {
                return $this->rows[$id] ?? null;
            }

            public function page(int $offset, int $limit): NotePage
            {
                $all = array_reverse(array_values($this->rows));

                return new NotePage(array_slice($all, $offset, $limit), count($all), $offset, $limit);
            }
        };
        $publisher = new class($this->published) implements NotePublisher
        {
            /** @param list<Note> $seen */
            public function __construct(private array &$seen) {} // @phpstan-ignore property.onlyWritten

            public function noteCreated(Note $note): void
            {
                $this->seen[] = $note;
            }
        };
        $this->service = new NoteService($repository, $publisher);
    }

    public function test_create_trims_stores_and_publishes(): void
    {
        $note = $this->service->create('  hello  ', null);

        $this->assertSame('hello', $note->title);
        $this->assertSame('', $note->body);
        $this->assertMatchesRegularExpression('/^[0-9a-f]{32}$/', $note->id);
        $this->assertSame($note, $this->service->get($note->id));
        $this->assertSame([$note], $this->published);
    }

    public function test_invalid_input_names_each_field_and_stores_nothing(): void
    {
        try {
            $this->service->create('   ', str_repeat('x', Note::BODY_MAX + 1));
            $this->fail('expected InvalidNote');
        } catch (InvalidNote $e) {
            $this->assertSame(['title', 'body'], array_keys($e->fields));
        }
        $this->assertSame(0, $this->service->list(0, 10)->total);
        $this->assertSame([], $this->published);
    }

    public function test_title_length_is_counted_in_characters_not_bytes(): void
    {
        $this->assertSame(200, mb_strlen($this->service->create(str_repeat('é', 200), null)->title));
        $this->expectException(InvalidNote::class);
        $this->service->create(str_repeat('é', 201), null);
    }

    public function test_get_of_a_missing_note_is_not_found(): void
    {
        $this->expectException(NoteNotFound::class);
        $this->service->get('nope');
    }

    public function test_list_is_newest_first_and_clamps_the_limit(): void
    {
        $first = $this->service->create('first', null);
        $second = $this->service->create('second', null);

        $page = $this->service->list(-5, 1000);
        $this->assertSame(0, $page->offset);
        $this->assertSame(NoteService::MAX_LIMIT, $page->limit);
        $this->assertSame([$second, $first], $page->items);
        $this->assertSame(2, $page->total);
    }
}
