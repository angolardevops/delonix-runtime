// The AMQP topology, declared the same way by the publisher and the consumer.
//
// One durable topic exchange carries `note.*`. The work queue dead-letters to a
// separate exchange whose only binding is the dead-letter queue, so a message
// the consumer rejects (poison, or retries exhausted) is kept for inspection
// instead of being dropped or requeued into a loop. Declarations are
// idempotent: whoever connects first creates them, the other finds them.
package messaging

import amqp "github.com/rabbitmq/amqp091-go"

// The exchange/queue names and the binding key the two sides agree on.
const (
	Exchange   = "notes.events"
	Queue      = "notes.created"
	BindingKey = "note.*"

	DLX = "notes.events.dlx"
	DLQ = "notes.created.dead"
)

// declareTopology declares the exchange, the dead-letter path and the work
// queue, and binds them. It returns the work queue.
func declareTopology(ch *amqp.Channel) (amqp.Queue, error) {
	if err := ch.ExchangeDeclare(Exchange, amqp.ExchangeTopic, true, false, false, false, nil); err != nil {
		return amqp.Queue{}, err
	}
	if err := ch.ExchangeDeclare(DLX, amqp.ExchangeFanout, true, false, false, false, nil); err != nil {
		return amqp.Queue{}, err
	}
	dead, err := ch.QueueDeclare(DLQ, true, false, false, false, nil)
	if err != nil {
		return amqp.Queue{}, err
	}
	if err := ch.QueueBind(dead.Name, "", DLX, false, nil); err != nil {
		return amqp.Queue{}, err
	}
	q, err := ch.QueueDeclare(Queue, true, false, false, false, amqp.Table{"x-dead-letter-exchange": DLX})
	if err != nil {
		return amqp.Queue{}, err
	}
	if err := ch.QueueBind(q.Name, BindingKey, Exchange, false, nil); err != nil {
		return amqp.Queue{}, err
	}
	return q, nil
}

// declareExchange declares the full topology (so the queue exists even if the
// consumer starts last, and a message published first is not lost) and is all
// the publisher needs.
func declareExchange(ch *amqp.Channel) error {
	_, err := declareTopology(ch)
	return err
}
