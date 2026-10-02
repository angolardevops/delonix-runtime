// Package memstore is the in-memory adapter of notes.Store. It is enough for
// the example and for tests; it keeps nothing across restarts. Swap it for a
// database adapter by implementing the same interface — the use cases do not
// change.
package memstore

import (
	"context"
	"sort"
	"sync"

	"__NAME__/internal/notes"
)

// Store keeps notes in a map guarded by a mutex.
type Store struct {
	mu    sync.RWMutex
	notes map[string]notes.Note
}

// New returns an empty store.
func New() *Store {
	return &Store{notes: map[string]notes.Note{}}
}

// Save stores or replaces a note.
func (s *Store) Save(_ context.Context, n notes.Note) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.notes[n.ID] = n
	return nil
}

// Get returns a note or notes.ErrNotFound.
func (s *Store) Get(_ context.Context, id string) (notes.Note, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	n, ok := s.notes[id]
	if !ok {
		return notes.Note{}, notes.ErrNotFound
	}
	return n, nil
}

// List returns a page, newest first (ties broken by id, so pages are stable).
func (s *Store) List(_ context.Context, offset, limit int) ([]notes.Note, int, error) {
	s.mu.RLock()
	all := make([]notes.Note, 0, len(s.notes))
	for _, n := range s.notes {
		all = append(all, n)
	}
	s.mu.RUnlock()
	sort.Slice(all, func(i, j int) bool {
		if !all[i].CreatedAt.Equal(all[j].CreatedAt) {
			return all[i].CreatedAt.After(all[j].CreatedAt)
		}
		return all[i].ID < all[j].ID
	})
	total := len(all)
	if offset >= total {
		return []notes.Note{}, total, nil
	}
	end := min(offset+limit, total)
	return all[offset:end], total, nil
}
