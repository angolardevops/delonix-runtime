package messaging

import (
	"context"
	"reflect"
	"sync"
	"testing"
	"time"

	"__NAME__/internal/notes"
)

var ctx = context.Background()

func aNote(id string) notes.Note {
	return notes.Note{ID: id, Title: "t", Body: "b", CreatedAt: time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)}
}

// ---- events ----

func TestEventRoundtripsAndNamesTheNote(t *testing.T) {
	event := NoteCreated(aNote("n1"))
	if event.Type != "note.created" || event.Data.ID != "n1" {
		t.Fatalf("unexpected event: %+v", event)
	}
	got, err := Decode(Encode(event))
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, event) {
		t.Fatalf("roundtrip changed the event: %+v != %+v", got, event)
	}
}

func TestDecodeRejectsAPoisonBody(t *testing.T) {
	for _, body := range [][]byte{
		[]byte("not json"),
		[]byte("123"),
		[]byte(`{"no":"type"}`),
		[]byte(`{"type":5}`),
	} {
		if _, err := Decode(body); err == nil {
			t.Errorf("poison body %q was accepted", body)
		}
	}
}

// ---- CompositePublisher ----

type recordingPublisher struct {
	mu   sync.Mutex
	seen []string
}

func (r *recordingPublisher) NoteCreated(_ context.Context, n notes.Note) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.seen = append(r.seen, n.ID)
}

func TestCompositeForwardsToEveryPublisher(t *testing.T) {
	a, b := &recordingPublisher{}, &recordingPublisher{}
	NewComposite(a, b).NoteCreated(ctx, aNote("x"))
	if len(a.seen) != 1 || a.seen[0] != "x" || len(b.seen) != 1 || b.seen[0] != "x" {
		t.Fatalf("composite did not fan out: a=%v b=%v", a.seen, b.seen)
	}
}

// ---- RabbitPublisher (fake sink) ----

type fakeSink struct {
	mu        sync.Mutex
	published [][2]string // routingKey, body
	failTimes int
	closed    bool
}

func (s *fakeSink) Publish(_ context.Context, routingKey string, body []byte) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.failTimes > 0 {
		s.failTimes--
		return context.DeadlineExceeded // any error reopens
	}
	s.published = append(s.published, [2]string{routingKey, string(body)})
	return nil
}

func (s *fakeSink) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.closed = true
	return nil
}

func (s *fakeSink) count() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.published)
}

func openerFor(sinks ...Sink) Opener {
	var mu sync.Mutex
	i := 0
	return func(context.Context, string) (Sink, error) {
		mu.Lock()
		defer mu.Unlock()
		s := sinks[i]
		i++
		return s, nil
	}
}

func immediateSleep(context.Context, time.Duration) {}

func TestPublisherDeliversQueuedNotes(t *testing.T) {
	sink := &fakeSink{}
	pub := NewRabbitPublisher("amqp://x", openerFor(sink), nil)
	pub.Start()
	pub.NoteCreated(ctx, aNote("n1"))
	pub.NoteCreated(ctx, aNote("n2"))
	cctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	if !pub.Close(cctx) {
		t.Fatal("publisher did not drain in time")
	}
	if sink.count() != 2 {
		t.Fatalf("published %d; want 2", sink.count())
	}
	e, err := Decode([]byte(sink.published[0][1]))
	if err != nil || e.Data.ID != "n1" {
		t.Fatalf("first delivery was %+v, %v", e, err)
	}
	if !sink.closed {
		t.Fatal("sink was not closed on drain")
	}
}

func TestPublisherReopensTheSinkAfterAFailure(t *testing.T) {
	broken, good := &fakeSink{failTimes: 1}, &fakeSink{}
	pub := NewRabbitPublisher("amqp://x", openerFor(broken, good), nil,
		WithSleep(immediateSleep), WithJitter(func() float64 { return 0 }))
	pub.Start()
	pub.NoteCreated(ctx, aNote("n1"))
	cctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	if !pub.Close(cctx) {
		t.Fatal("publisher did not drain in time")
	}
	if good.count() != 1 {
		t.Fatalf("reopened sink delivered %d; want 1", good.count())
	}
}

func TestPublisherDropsWhenTheQueueIsFull(t *testing.T) {
	sink := &fakeSink{}
	pub := NewRabbitPublisher("amqp://x", openerFor(sink), nil, WithQueueSize(1))
	// No worker yet: the queue fills and the extra notes are dropped, never block.
	pub.NoteCreated(ctx, aNote("n1"))
	pub.NoteCreated(ctx, aNote("n2"))
	pub.NoteCreated(ctx, aNote("n3"))
	pub.Start()
	cctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	pub.Close(cctx)
	if sink.count() != 1 {
		t.Fatalf("published %d; want 1 (two were dropped)", sink.count())
	}
}

// ---- RabbitConsumer.Handle (fake delivery) ----

type fakeIncoming struct {
	body         []byte
	retry        int
	acked        bool
	deadLettered bool
	retriedWith  time.Duration
	retriedOK    bool
}

func (m *fakeIncoming) Body() []byte      { return m.body }
func (m *fakeIncoming) RetryCount() int   { return m.retry }
func (m *fakeIncoming) Ack() error        { m.acked = true; return nil }
func (m *fakeIncoming) DeadLetter() error { m.deadLettered = true; return nil }
func (m *fakeIncoming) Retry(d time.Duration) error {
	m.retriedWith = d
	m.retriedOK = true
	return nil
}

func okHandler(context.Context, Event) error   { return nil }
func boomHandler(context.Context, Event) error { return context.DeadlineExceeded }

func TestHandleAcksAProcessedMessage(t *testing.T) {
	msg := &fakeIncoming{body: Encode(NoteCreated(aNote("n1")))}
	if err := NewRabbitConsumer("amqp://x", okHandler, nil).Handle(ctx, msg); err != nil {
		t.Fatal(err)
	}
	if !msg.acked || msg.deadLettered {
		t.Fatalf("expected ack: %+v", msg)
	}
}

func TestHandleDeadLettersAPoisonMessageAtOnce(t *testing.T) {
	msg := &fakeIncoming{body: []byte("not an event")}
	_ = NewRabbitConsumer("amqp://x", okHandler, nil).Handle(ctx, msg)
	if !msg.deadLettered || msg.acked || msg.retriedOK {
		t.Fatalf("expected dead-letter: %+v", msg)
	}
}

func TestHandleRetriesATransientFailure(t *testing.T) {
	msg := &fakeIncoming{body: Encode(NoteCreated(aNote("n1"))), retry: 0}
	c := NewRabbitConsumer("amqp://x", boomHandler, nil, WithMaxRetries(3), WithRetryDelay(250*time.Millisecond))
	_ = c.Handle(ctx, msg)
	if !msg.retriedOK || msg.retriedWith != 250*time.Millisecond || msg.deadLettered || msg.acked {
		t.Fatalf("expected retry at 250ms: %+v", msg)
	}
}

func TestHandleDeadLettersWhenRetriesAreExhausted(t *testing.T) {
	msg := &fakeIncoming{body: Encode(NoteCreated(aNote("n1"))), retry: 3}
	c := NewRabbitConsumer("amqp://x", boomHandler, nil, WithMaxRetries(3))
	_ = c.Handle(ctx, msg)
	if !msg.deadLettered || msg.acked || msg.retriedOK {
		t.Fatalf("expected dead-letter when exhausted: %+v", msg)
	}
}
