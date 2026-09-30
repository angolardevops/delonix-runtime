// Package notes is the example capability: create, read and list short
// notes. It holds the rules and the use cases, and names what it needs from
// the outside world as two small interfaces (Store, Publisher). It imports
// nothing about HTTP, telemetry or storage technology — the architecture test
// in internal/archtest fails the build if it ever does.
package notes

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"strings"
	"time"
	"unicode/utf8"
)

// Limits of a note, enforced by the use case (not by the transport).
const (
	MaxTitleLen = 200
	MaxBodyLen  = 10_000
	MaxPageSize = 100
)

// Note is one stored note.
type Note struct {
	ID        string    `json:"id"`
	Title     string    `json:"title"`
	Body      string    `json:"body"`
	CreatedAt time.Time `json:"created_at"`
}

// ErrNotFound: no note with that id.
var ErrNotFound = errors.New("note not found")

// ValidationError lists every field that failed, so a client fixes a request
// in one round trip.
type ValidationError struct {
	Fields map[string]string
}

func (e *ValidationError) Error() string {
	parts := make([]string, 0, len(e.Fields))
	for f, msg := range e.Fields {
		parts = append(parts, f+": "+msg)
	}
	return "invalid note: " + strings.Join(parts, "; ")
}

// Store is the persistence port. The in-memory adapter lives in
// internal/memstore; a database adapter implements the same three methods.
type Store interface {
	Save(ctx context.Context, n Note) error
	Get(ctx context.Context, id string) (Note, error)
	// List returns one page, newest first, and the total count.
	List(ctx context.Context, offset, limit int) ([]Note, int, error)
}

// Publisher tells the outside world that a note exists. It must not block
// the caller on the network: the outbound webhook adapter queues.
type Publisher interface {
	NoteCreated(ctx context.Context, n Note)
}

// Service holds the use cases.
type Service struct {
	store Store
	pub   Publisher
	now   func() time.Time
	newID func() string
}

// NewService wires the use cases to their ports. A nil Publisher publishes
// nothing.
func NewService(store Store, pub Publisher) *Service {
	return &Service{store: store, pub: pub, now: time.Now, newID: randomID}
}

// CreateInput is what a caller may set.
type CreateInput struct {
	Title string
	Body  string
}

// Create validates and stores a note, then publishes it.
func (s *Service) Create(ctx context.Context, in CreateInput) (Note, error) {
	title := strings.TrimSpace(in.Title)
	fields := map[string]string{}
	switch {
	case title == "":
		fields["title"] = "is required"
	case utf8.RuneCountInString(title) > MaxTitleLen:
		fields["title"] = fmt.Sprintf("must be at most %d characters", MaxTitleLen)
	}
	if utf8.RuneCountInString(in.Body) > MaxBodyLen {
		fields["body"] = fmt.Sprintf("must be at most %d characters", MaxBodyLen)
	}
	if len(fields) > 0 {
		return Note{}, &ValidationError{Fields: fields}
	}
	n := Note{ID: s.newID(), Title: title, Body: in.Body, CreatedAt: s.now().UTC()}
	if err := s.store.Save(ctx, n); err != nil {
		return Note{}, fmt.Errorf("saving note: %w", err)
	}
	if s.pub != nil {
		s.pub.NoteCreated(ctx, n)
	}
	return n, nil
}

// Get returns one note or ErrNotFound.
func (s *Service) Get(ctx context.Context, id string) (Note, error) {
	return s.store.Get(ctx, id)
}

// Page is one page of a listing.
type Page struct {
	Items  []Note `json:"items"`
	Total  int    `json:"total"`
	Offset int    `json:"offset"`
	Limit  int    `json:"limit"`
}

// List returns a page. limit is clamped to 1..MaxPageSize, offset to >= 0.
func (s *Service) List(ctx context.Context, offset, limit int) (Page, error) {
	if limit < 1 || limit > MaxPageSize {
		return Page{}, &ValidationError{Fields: map[string]string{
			"limit": fmt.Sprintf("must be between 1 and %d", MaxPageSize),
		}}
	}
	if offset < 0 {
		return Page{}, &ValidationError{Fields: map[string]string{"offset": "must not be negative"}}
	}
	items, total, err := s.store.List(ctx, offset, limit)
	if err != nil {
		return Page{}, fmt.Errorf("listing notes: %w", err)
	}
	return Page{Items: items, Total: total, Offset: offset, Limit: limit}, nil
}

func randomID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(fmt.Sprintf("crypto/rand failed: %v", err))
	}
	return hex.EncodeToString(b[:])
}
