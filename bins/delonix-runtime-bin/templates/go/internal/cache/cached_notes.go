// Cache-aside over the notes use cases: Get(id) reads through the cache.
//
// Wraps the use cases from the OUTSIDE, exactly like telemetry.TracedNotes, so
// the notes package never imports the cache. A note is immutable once created,
// so a cached note is never stale — only the TTL expires it. Only Get is
// cached: List changes whenever a note is created, and Create returns a note
// the caller already holds (it is written to the cache so the first read is a
// hit). The cache is a slow path, never a failure path — Cache swallows backend
// errors, and a decode error here falls through to the store too.
package cache

import (
	"context"
	"encoding/json"
	"log/slog"
	"time"

	"__NAME__/internal/notes"
)

const keyPrefix = "note:v1:"

// Notes is the use-case surface this decorator wraps. Both notes.Service and
// telemetry.TracedNotes fit it, so either can be wrapped without the notes
// package knowing a cache exists.
type Notes interface {
	Create(ctx context.Context, in notes.CreateInput) (notes.Note, error)
	Get(ctx context.Context, id string) (notes.Note, error)
	List(ctx context.Context, offset, limit int) (notes.Page, error)
}

// CachedNotes is a Notes that reads Get through a Cache.
type CachedNotes struct {
	inner Notes
	cache Cache
	ttl   time.Duration
	log   *slog.Logger
}

// NewCachedNotes wraps inner with a read-through cache. A nil logger uses the
// default.
func NewCachedNotes(inner Notes, c Cache, ttl time.Duration, log *slog.Logger) *CachedNotes {
	if log == nil {
		log = slog.Default()
	}
	return &CachedNotes{inner: inner, cache: c, ttl: ttl, log: log}
}

func (n *CachedNotes) Create(ctx context.Context, in notes.CreateInput) (notes.Note, error) {
	note, err := n.inner.Create(ctx, in)
	if err != nil {
		return notes.Note{}, err
	}
	n.cache.Set(ctx, keyPrefix+note.ID, encodeNote(note), n.ttl)
	return note, nil
}

func (n *CachedNotes) Get(ctx context.Context, id string) (notes.Note, error) {
	key := keyPrefix + id
	if raw, ok := n.cache.Get(ctx, key); ok {
		if note, err := decodeNote(raw); err == nil {
			return note, nil
		}
		// A corrupt or old-shape entry: ignore it and read the store.
		n.log.WarnContext(ctx, "discarding an undecodable cache entry", "note_id", id)
	}
	note, err := n.inner.Get(ctx, id) // returns notes.ErrNotFound on a miss
	if err != nil {
		return notes.Note{}, err
	}
	n.cache.Set(ctx, key, encodeNote(note), n.ttl)
	return note, nil
}

func (n *CachedNotes) List(ctx context.Context, offset, limit int) (notes.Page, error) {
	return n.inner.List(ctx, offset, limit)
}

func encodeNote(n notes.Note) []byte {
	b, _ := json.Marshal(n)
	return b
}

func decodeNote(raw []byte) (notes.Note, error) {
	var n notes.Note
	if err := json.Unmarshal(raw, &n); err != nil {
		return notes.Note{}, err
	}
	return n, nil
}

var _ Notes = (*CachedNotes)(nil)
