// Consume `note.created` from RabbitMQ, with manual acks and a bounded retry.
//
// The per-message decision — acknowledge, retry, or dead-letter — lives in
// RabbitConsumer.Handle, which talks to an Incoming message (Ack / Retry /
// DeadLetter), so it is tested with no broker. The connect-and-consume loop in
// run is the thin glue that wraps each real delivery as an Incoming.
//
// Rules:
//   - the handler returns nil -> ack (processed; at-least-once, so make it
//     idempotent);
//   - the body is not a valid event -> dead-letter at once (a retry cannot fix
//     it);
//   - the handler returns an error -> retry up to maxRetries (republished with
//     an incremented x-retry header), then dead-letter — a poison message can
//     never loop for ever.
package messaging

import (
	"context"
	"log/slog"
	"time"

	amqp "github.com/rabbitmq/amqp091-go"
)

// Incoming is one delivery, abstracted from AMQP so Handle is broker-free.
type Incoming interface {
	Body() []byte
	// RetryCount is the x-retry header, 0 on first delivery.
	RetryCount() int
	Ack() error
	// DeadLetter rejects without requeue — routed to the dead-letter exchange.
	DeadLetter() error
	// Retry republishes with x-retry incremented, then acks the original.
	Retry(delay time.Duration) error
}

// Handler processes one decoded event. Returning an error means "transient
// failure, retry".
type Handler func(ctx context.Context, e Event) error

// LogHandler is the default handler: it records receipt. Replace it with the
// real side-effect (a projection, an index write, a notification).
func LogHandler(log *slog.Logger) Handler {
	return func(ctx context.Context, e Event) error {
		log.InfoContext(ctx, "note.created consumed", "note_id", e.Data.ID)
		return nil
	}
}

// RabbitConsumer consumes the work queue.
type RabbitConsumer struct {
	url        string
	handler    Handler
	maxRetries int
	retryDelay time.Duration
	prefetch   int
	log        *slog.Logger
	onOutcome  func(outcome string)

	conn   *amqp.Connection
	cancel context.CancelFunc
	done   chan struct{}
}

// ConsumerOption tunes a RabbitConsumer.
type ConsumerOption func(*RabbitConsumer)

// WithMaxRetries sets how often a transient failure is retried (default 3).
func WithMaxRetries(n int) ConsumerOption {
	return func(c *RabbitConsumer) {
		if n >= 0 {
			c.maxRetries = n
		}
	}
}

// WithRetryDelay sets the delay before a retry (default 1s).
func WithRetryDelay(d time.Duration) ConsumerOption {
	return func(c *RabbitConsumer) { c.retryDelay = d }
}

// WithPrefetch sets the channel QoS prefetch (default 16).
func WithPrefetch(n int) ConsumerOption {
	return func(c *RabbitConsumer) {
		if n > 0 {
			c.prefetch = n
		}
	}
}

// WithConsumerOutcome registers a callback counted per outcome
// ("acked", "retried", "dead_lettered", "poison") — wire it to a metric.
func WithConsumerOutcome(fn func(outcome string)) ConsumerOption {
	return func(c *RabbitConsumer) {
		if fn != nil {
			c.onOutcome = fn
		}
	}
}

// NewRabbitConsumer builds a consumer. Call Start to connect and consume.
func NewRabbitConsumer(url string, handler Handler, log *slog.Logger, opts ...ConsumerOption) *RabbitConsumer {
	if log == nil {
		log = slog.Default()
	}
	c := &RabbitConsumer{
		url:        url,
		handler:    handler,
		maxRetries: 3,
		retryDelay: time.Second,
		prefetch:   16,
		log:        log,
		onOutcome:  func(string) {},
	}
	for _, o := range opts {
		o(c)
	}
	return c
}

// Handle decides the fate of one delivery. It never leaves a delivery
// unanswered: every path ends in an ack, a dead-letter, or a retry. The error
// it returns is only an I/O error from that final action, never the handler's.
func (c *RabbitConsumer) Handle(ctx context.Context, msg Incoming) error {
	event, err := Decode(msg.Body())
	if err != nil {
		c.onOutcome("poison")
		c.log.WarnContext(ctx, "dead-lettering an invalid message", "error", err.Error())
		return msg.DeadLetter()
	}
	if err := c.handler(ctx, event); err != nil {
		if msg.RetryCount() >= c.maxRetries {
			c.onOutcome("dead_lettered")
			c.log.ErrorContext(ctx, "handler failed, retries exhausted, dead-lettering",
				"error", err.Error(), "retries", msg.RetryCount())
			return msg.DeadLetter()
		}
		c.onOutcome("retried")
		c.log.WarnContext(ctx, "handler failed, retrying",
			"error", err.Error(), "attempt", msg.RetryCount()+1)
		return msg.Retry(c.retryDelay)
	}
	c.onOutcome("acked")
	return msg.Ack()
}

// ---- AMQP glue (not unit-tested; exercised by the CI integration run) ----

// Start connects and consumes in the background. A down broker does not crash
// startup: it logs and returns, and nothing is consumed until the next start.
func (c *RabbitConsumer) Start(ctx context.Context) {
	cctx, cancel := context.WithCancel(ctx)
	c.cancel = cancel
	c.done = make(chan struct{})
	go c.run(cctx)
}

// Close stops consuming and closes the connection.
func (c *RabbitConsumer) Close(context.Context) error {
	if c.cancel != nil {
		c.cancel()
	}
	var err error
	if c.conn != nil {
		err = c.conn.Close()
	}
	if c.done != nil {
		<-c.done
	}
	return err
}

func (c *RabbitConsumer) run(ctx context.Context) {
	defer close(c.done)
	conn, err := amqp.Dial(c.url)
	if err != nil {
		c.log.WarnContext(ctx, "rabbit consumer could not connect; messages will not be consumed",
			"error", err.Error())
		return
	}
	c.conn = conn
	ch, err := conn.Channel()
	if err != nil {
		c.log.WarnContext(ctx, "rabbit consumer channel failed", "error", err.Error())
		return
	}
	if err := ch.Qos(c.prefetch, 0, false); err != nil {
		c.log.WarnContext(ctx, "rabbit consumer qos failed", "error", err.Error())
		return
	}
	q, err := declareTopology(ch)
	if err != nil {
		c.log.WarnContext(ctx, "rabbit consumer topology failed", "error", err.Error())
		return
	}
	deliveries, err := ch.Consume(q.Name, "", false, false, false, false, nil)
	if err != nil {
		c.log.WarnContext(ctx, "rabbit consumer consume failed", "error", err.Error())
		return
	}
	c.log.InfoContext(ctx, "rabbit consumer ready")
	for {
		select {
		case <-ctx.Done():
			return
		case d, ok := <-deliveries:
			if !ok {
				return
			}
			_ = c.Handle(ctx, &amqpIncoming{d: d, ch: ch})
		}
	}
}

type amqpIncoming struct {
	d  amqp.Delivery
	ch *amqp.Channel
}

func (m *amqpIncoming) Body() []byte { return m.d.Body }

func (m *amqpIncoming) RetryCount() int {
	switch n := m.d.Headers["x-retry"].(type) {
	case int:
		return n
	case int32:
		return int(n)
	case int64:
		return int(n)
	default:
		return 0
	}
}

func (m *amqpIncoming) Ack() error        { return m.d.Ack(false) }
func (m *amqpIncoming) DeadLetter() error { return m.d.Reject(false) }

func (m *amqpIncoming) Retry(delay time.Duration) error {
	time.Sleep(delay)
	err := m.ch.PublishWithContext(context.Background(), Exchange, RoutingKey, false, false, amqp.Publishing{
		DeliveryMode: amqp.Persistent,
		ContentType:  "application/json",
		Body:         m.d.Body,
		Headers:      amqp.Table{"x-retry": int32(m.RetryCount() + 1)},
	})
	if err != nil {
		return err
	}
	return m.d.Ack(false)
}

var _ Incoming = (*amqpIncoming)(nil)
