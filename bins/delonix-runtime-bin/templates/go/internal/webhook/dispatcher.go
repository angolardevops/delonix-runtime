package webhook

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"math/rand/v2"
	"net/http"
	"sync"
	"time"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/metric"
	"go.opentelemetry.io/otel/trace"

	"__NAME__/internal/notes"
)

// Event is what the receiver gets as the JSON body.
type Event struct {
	Type string     `json:"type"`
	Data notes.Note `json:"data"`
}

// DispatcherConfig bounds every dimension of delivery.
type DispatcherConfig struct {
	URL         string
	Key         []byte
	Client      *http.Client  // its Transport should propagate trace context
	Timeout     time.Duration // per attempt
	MaxAttempts int
	BaseBackoff time.Duration
	QueueSize   int
}

// Dispatcher delivers events to one URL in the background: a bounded queue,
// one worker, bounded attempts with exponential backoff and full jitter.
//
// Delivery is AT-LEAST-ONCE while the process lives and AT-MOST-ONCE across a
// restart: the queue is memory, so what is queued when the process dies is
// lost, and a full queue drops (and logs, and counts) instead of blocking the
// request that created the note. The webhook-id is the note id, so a
// receiver that deduplicates by it sees each note once however many attempts
// it took. A deployment that must not lose an event needs an outbox table
// written in the same transaction as the note — see docs/adr.
type Dispatcher struct {
	cfg     DispatcherConfig
	log     *slog.Logger
	queue   chan job
	done    chan struct{}
	mu      sync.RWMutex // guards closed against a send on a closed queue
	closed  bool
	outcome metric.Int64Counter
	now     func() time.Time
	sleep   func(context.Context, time.Duration) error
}

type job struct {
	ev   Event
	link trace.SpanContext
}

// NewDispatcher starts the worker.
func NewDispatcher(cfg DispatcherConfig, log *slog.Logger) *Dispatcher {
	counter, _ := otel.Meter("__NAME__/webhook").Int64Counter(
		"webhook.deliveries",
		metric.WithDescription("Outbound webhook deliveries by final outcome"),
	)
	d := &Dispatcher{
		cfg:     cfg,
		log:     log,
		queue:   make(chan job, cfg.QueueSize),
		done:    make(chan struct{}),
		outcome: counter,
		now:     time.Now,
		sleep:   sleepCtx,
	}
	go d.run()
	return d
}

// NoteCreated implements notes.Publisher. It never blocks on the network.
func (d *Dispatcher) NoteCreated(ctx context.Context, n notes.Note) {
	d.mu.RLock()
	defer d.mu.RUnlock()
	if d.closed {
		d.count(context.Background(), "dropped")
		d.log.WarnContext(ctx, "webhook dispatcher closed, event dropped", "note_id", n.ID)
		return
	}
	select {
	case d.queue <- job{ev: Event{Type: "note.created", Data: n}, link: trace.SpanContextFromContext(ctx)}:
	default:
		d.count(context.Background(), "dropped")
		d.log.ErrorContext(ctx, "webhook queue full, event dropped", "note_id", n.ID)
	}
}

// Close stops taking events and waits for the queue to drain, up to ctx's
// deadline. What is still queued then is dropped and counted.
func (d *Dispatcher) Close(ctx context.Context) error {
	d.mu.Lock()
	if !d.closed {
		d.closed = true
		close(d.queue)
	}
	d.mu.Unlock()
	select {
	case <-d.done:
		return nil
	case <-ctx.Done():
		return fmt.Errorf("webhook queue not drained: %w", ctx.Err())
	}
}

func (d *Dispatcher) run() {
	defer close(d.done)
	for j := range d.queue {
		d.deliver(j)
	}
}

func (d *Dispatcher) deliver(j job) {
	// A child of the request that created the note, so the delivery and
	// the HTTP call it makes share that request's trace id. The request's
	// own context is not reused: it is cancelled as soon as the response is
	// written, and the delivery outlives it.
	parent := trace.ContextWithSpanContext(context.Background(), j.link)
	ctx, span := otel.Tracer("__NAME__/webhook").Start(parent, "webhook.deliver",
		trace.WithSpanKind(trace.SpanKindProducer),
		trace.WithAttributes(attribute.String("webhook.event", j.ev.Type)))
	defer span.End()

	body, err := json.Marshal(j.ev)
	if err != nil {
		d.fail(ctx, span, j, 0, err)
		return
	}
	var lastErr error
	for attempt := 1; attempt <= d.cfg.MaxAttempts; attempt++ {
		retry, err := d.attempt(ctx, j.ev.Data.ID, body)
		if err == nil {
			span.SetAttributes(attribute.Int("webhook.attempts", attempt))
			d.count(ctx, "delivered")
			d.log.InfoContext(ctx, "webhook delivered", "note_id", j.ev.Data.ID, "attempts", attempt)
			return
		}
		lastErr = err
		if !retry || attempt == d.cfg.MaxAttempts {
			break
		}
		// Full jitter: uniform in [0, base * 2^(attempt-1)].
		backoff := time.Duration(rand.Int64N(int64(d.cfg.BaseBackoff) << (attempt - 1)))
		if d.sleep(ctx, backoff) != nil {
			break
		}
	}
	d.fail(ctx, span, j, d.cfg.MaxAttempts, lastErr)
}

// attempt sends once. retry reports whether trying again could help.
func (d *Dispatcher) attempt(ctx context.Context, id string, body []byte) (retry bool, err error) {
	ctx, cancel := context.WithTimeout(ctx, d.cfg.Timeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, d.cfg.URL, bytes.NewReader(body))
	if err != nil {
		return false, err
	}
	now := d.now()
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set(HeaderID, id)
	req.Header.Set(HeaderTimestamp, fmt.Sprint(now.Unix()))
	req.Header.Set(HeaderSignature, Sign(d.cfg.Key, id, now, body))
	resp, err := d.cfg.Client.Do(req)
	if err != nil {
		return true, err // network error or timeout
	}
	resp.Body.Close()
	switch {
	case resp.StatusCode >= 200 && resp.StatusCode < 300:
		return false, nil
	case resp.StatusCode == http.StatusTooManyRequests || resp.StatusCode >= 500:
		return true, fmt.Errorf("receiver answered %d", resp.StatusCode)
	default:
		// 4xx: the receiver refused this message; resending it will not help.
		return false, fmt.Errorf("receiver answered %d", resp.StatusCode)
	}
}

func (d *Dispatcher) fail(ctx context.Context, span trace.Span, j job, attempts int, err error) {
	span.RecordError(err)
	span.SetStatus(codes.Error, "delivery failed")
	d.count(ctx, "failed")
	d.log.ErrorContext(ctx, "webhook delivery failed", "note_id", j.ev.Data.ID, "attempts", attempts, "error", err.Error())
}

func (d *Dispatcher) count(ctx context.Context, outcome string) {
	d.outcome.Add(ctx, 1, metric.WithAttributes(attribute.String("outcome", outcome)))
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}
