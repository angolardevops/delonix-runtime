// Publish `note.created` to RabbitMQ without ever blocking a request.
//
// RabbitPublisher implements notes.Publisher: NoteCreated only enqueues (and
// returns), a single worker goroutine drains the queue to the broker. The queue
// is bounded and drops — never blocks — when full, exactly like the webhook
// dispatcher. The AMQP specifics live behind Sink, injected, so the
// queue/worker/backoff logic is tested with no broker; the default sink
// (DialSink) opens a topic exchange with persistent messages.
//
// CompositePublisher fans one note out to several publishers (the webhook
// dispatcher AND this one), so wiring both does not mean choosing between them.
package messaging

import (
	"context"
	"log/slog"
	"math/rand/v2"
	"time"

	amqp "github.com/rabbitmq/amqp091-go"

	"__NAME__/internal/notes"
)

// Sink is where the worker publishes. The default is an AMQP exchange; a test
// passes a fake. Publish returns an error on a broker failure — the worker then
// reopens the sink.
type Sink interface {
	Publish(ctx context.Context, routingKey string, body []byte) error
	Close() error
}

// Opener opens a Sink for a URL. Injected, so the worker is broker-free in tests.
type Opener func(ctx context.Context, url string) (Sink, error)

// CompositePublisher forwards a note to several publishers. One failing to
// enqueue never stops the others — each is already non-blocking and bounded.
type CompositePublisher struct {
	publishers []notes.Publisher
}

// NewComposite builds a CompositePublisher.
func NewComposite(publishers ...notes.Publisher) *CompositePublisher {
	return &CompositePublisher{publishers: publishers}
}

// NoteCreated forwards to every wrapped publisher.
func (c *CompositePublisher) NoteCreated(ctx context.Context, n notes.Note) {
	for _, p := range c.publishers {
		p.NoteCreated(ctx, n)
	}
}

type job struct {
	routingKey string
	body       []byte
}

// RabbitPublisher is a notes.Publisher that enqueues and drains to a Sink.
type RabbitPublisher struct {
	url    string
	opener Opener
	log    *slog.Logger

	queue     chan job
	stop      chan struct{} // closed by Close: worker drains then exits
	done      chan struct{} // closed by the worker on exit
	cancel    context.CancelFunc
	workCtx   context.Context
	closed    bool
	onOutcome func(outcome string)

	baseBackoff time.Duration
	maxBackoff  time.Duration
	sleep       func(ctx context.Context, d time.Duration)
	jitter      func() float64
}

// PublisherOption tunes a RabbitPublisher.
type PublisherOption func(*RabbitPublisher)

// WithQueueSize bounds the publish queue (default 256).
func WithQueueSize(n int) PublisherOption {
	return func(p *RabbitPublisher) {
		if n > 0 {
			p.queue = make(chan job, n)
		}
	}
}

// WithBackoff sets the reopen backoff floor and ceiling.
func WithBackoff(base, max time.Duration) PublisherOption {
	return func(p *RabbitPublisher) { p.baseBackoff, p.maxBackoff = base, max }
}

// WithSleep injects the backoff sleep (tests pass an immediate one).
func WithSleep(fn func(ctx context.Context, d time.Duration)) PublisherOption {
	return func(p *RabbitPublisher) {
		if fn != nil {
			p.sleep = fn
		}
	}
}

// WithJitter injects the backoff jitter in [0,1) (tests pass a constant).
func WithJitter(fn func() float64) PublisherOption {
	return func(p *RabbitPublisher) {
		if fn != nil {
			p.jitter = fn
		}
	}
}

// WithOutcome registers a callback counted per publish outcome
// ("published", "failed", "dropped") — wire it to a metric.
func WithOutcome(fn func(outcome string)) PublisherOption {
	return func(p *RabbitPublisher) {
		if fn != nil {
			p.onOutcome = fn
		}
	}
}

// NewRabbitPublisher builds a publisher. Call Start before NoteCreated.
func NewRabbitPublisher(url string, opener Opener, log *slog.Logger, opts ...PublisherOption) *RabbitPublisher {
	if log == nil {
		log = slog.Default()
	}
	p := &RabbitPublisher{
		url:         url,
		opener:      opener,
		log:         log,
		queue:       make(chan job, 256),
		stop:        make(chan struct{}),
		done:        make(chan struct{}),
		onOutcome:   func(string) {},
		baseBackoff: 500 * time.Millisecond,
		maxBackoff:  10 * time.Second,
		sleep:       ctxSleep,
		jitter:      rand.Float64,
	}
	for _, o := range opts {
		o(p)
	}
	return p
}

// Start launches the worker goroutine.
func (p *RabbitPublisher) Start() {
	p.workCtx, p.cancel = context.WithCancel(context.Background())
	go p.run()
}

// NoteCreated enqueues the event. It never blocks: a full queue drops the event.
func (p *RabbitPublisher) NoteCreated(_ context.Context, n notes.Note) {
	if p.closed {
		p.onOutcome("dropped")
		return
	}
	j := job{routingKey: RoutingKey, body: Encode(NoteCreated(n))}
	select {
	case p.queue <- j:
	default:
		p.onOutcome("dropped")
		p.log.Error("rabbit publish queue full, event dropped", "note_id", n.ID)
	}
}

// Close stops accepting, drains what is queued within ctx, and reports whether
// the queue drained before the deadline. A dead broker is bounded by ctx: the
// worker is cancelled and Close returns false.
func (p *RabbitPublisher) Close(ctx context.Context) bool {
	p.closed = true
	close(p.stop)
	select {
	case <-p.done:
		return true
	case <-ctx.Done():
		if p.cancel != nil {
			p.cancel()
		}
		<-p.done
		return false
	}
}

func (p *RabbitPublisher) run() {
	defer close(p.done)
	var sink Sink
	defer func() {
		if sink != nil {
			_ = sink.Close()
		}
	}()
	drain := false
	for {
		var j job
		if drain {
			select {
			case j = <-p.queue:
			default:
				return
			}
		} else {
			select {
			case j = <-p.queue:
			case <-p.stop:
				drain = true
				continue
			case <-p.workCtx.Done():
				return
			}
		}
		sink = p.publish(sink, j)
		if sink == nil && p.workCtx.Err() != nil {
			return // cancelled mid-retry
		}
	}
}

// publish delivers one job, reopening the sink with capped backoff on failure.
// It retries until it succeeds or the worker context is cancelled. Returns the
// (possibly reopened) sink, or nil if it gave up because the context ended.
func (p *RabbitPublisher) publish(sink Sink, j job) Sink {
	attempt := 0
	for {
		if p.workCtx.Err() != nil {
			return nil
		}
		if sink == nil {
			s, err := p.opener(p.workCtx, p.url)
			if err != nil {
				p.onOutcome("failed")
				p.backoff(&attempt)
				continue
			}
			sink = s
		}
		if err := sink.Publish(p.workCtx, j.routingKey, j.body); err != nil {
			p.log.Warn("rabbit sink error, reopening", "error", err.Error())
			_ = sink.Close()
			sink = nil
			p.onOutcome("failed")
			p.backoff(&attempt)
			continue
		}
		p.onOutcome("published")
		return sink
	}
}

func (p *RabbitPublisher) backoff(attempt *int) {
	*attempt++
	d := p.baseBackoff
	for i := 1; i < *attempt; i++ {
		d *= 2
		if d >= p.maxBackoff {
			d = p.maxBackoff
			break
		}
	}
	p.sleep(p.workCtx, time.Duration(p.jitter()*float64(d)))
}

func ctxSleep(ctx context.Context, d time.Duration) {
	if d <= 0 {
		return
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
	case <-t.C:
	}
}

var _ notes.Publisher = (*CompositePublisher)(nil)
var _ notes.Publisher = (*RabbitPublisher)(nil)

// ---- AMQP glue (not unit-tested; exercised by the CI integration run) ----

// DialSink is the default Opener: it dials the broker, opens a channel,
// declares the topology, and returns a Sink that publishes persistent messages
// to the topic exchange.
func DialSink(_ context.Context, url string) (Sink, error) {
	conn, err := amqp.Dial(url)
	if err != nil {
		return nil, err
	}
	ch, err := conn.Channel()
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	if err := declareExchange(ch); err != nil {
		_ = conn.Close()
		return nil, err
	}
	return &amqpSink{conn: conn, ch: ch}, nil
}

type amqpSink struct {
	conn *amqp.Connection
	ch   *amqp.Channel
}

func (s *amqpSink) Publish(ctx context.Context, routingKey string, body []byte) error {
	return s.ch.PublishWithContext(ctx, Exchange, routingKey, false, false, amqp.Publishing{
		DeliveryMode: amqp.Persistent,
		ContentType:  "application/json",
		Body:         body,
	})
}

func (s *amqpSink) Close() error { return s.conn.Close() }
