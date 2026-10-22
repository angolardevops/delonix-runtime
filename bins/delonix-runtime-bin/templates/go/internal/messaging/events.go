// Package messaging is the RabbitMQ pub/sub for `note.created`: a non-blocking
// publisher and a consumer with manual acks and a bounded retry. It names the
// wire format once (events.go) so the two sides cannot drift, keeps the
// ack/retry/dead-letter decision broker-free (so it is unit-tested with a fake
// delivery), and hides the AMQP specifics behind small interfaces (Sink,
// Incoming). It wraps the notes use cases from the OUTSIDE — the notes package
// never imports it (the architecture test enforces that).
package messaging

import (
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"__NAME__/internal/notes"
)

// EventType and RoutingKey name the one event. A consumer binds `note.*` or
// `note.created`. The envelope (type + data) matches the outbound webhook's,
// so a receiver sees the same shape on either transport.
const (
	EventType  = "note.created"
	RoutingKey = "note.created"
)

// Event is `note.created` on the wire.
type Event struct {
	Type string   `json:"type"`
	Data NoteData `json:"data"`
}

// NoteData is the note as it travels (created_at as an RFC3339 string).
type NoteData struct {
	ID        string `json:"id"`
	Title     string `json:"title"`
	Body      string `json:"body"`
	CreatedAt string `json:"created_at"`
}

// NoteCreated builds the event for a note.
func NoteCreated(n notes.Note) Event {
	return Event{
		Type: EventType,
		Data: NoteData{
			ID:        n.ID,
			Title:     n.Title,
			Body:      n.Body,
			CreatedAt: n.CreatedAt.UTC().Format(time.RFC3339Nano),
		},
	}
}

// Encode is the compact JSON body of an event.
func Encode(e Event) []byte {
	b, _ := json.Marshal(e)
	return b
}

// Decode parses a message body. It returns an error on anything that is not a
// JSON object with a non-empty string `type` — a poison message the consumer
// dead-letters.
func Decode(body []byte) (Event, error) {
	var probe map[string]json.RawMessage
	if err := json.Unmarshal(body, &probe); err != nil {
		return Event{}, fmt.Errorf("not a JSON object: %w", err)
	}
	var typ string
	if err := json.Unmarshal(probe["type"], &typ); err != nil || typ == "" {
		return Event{}, errors.New("not a valid event envelope")
	}
	var e Event
	if err := json.Unmarshal(body, &e); err != nil {
		return Event{}, err
	}
	return e, nil
}
