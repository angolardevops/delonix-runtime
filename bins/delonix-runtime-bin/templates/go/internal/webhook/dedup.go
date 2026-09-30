package webhook

import (
	"sync"
	"time"
)

// Dedup remembers the ids of accepted deliveries for the tolerance window,
// so a sender's retry of a message already handled is acknowledged without
// being handled twice.
//
// It lives in memory: it does NOT survive a restart and is not shared
// between replicas. That is enough against the retries of a single sender to
// a single process; a deployment that must never process a message twice
// needs a durable store (a unique key in the database) instead.
type Dedup struct {
	mu   sync.Mutex
	seen map[string]time.Time
	max  int
}

// NewDedup keeps at most max ids.
func NewDedup(max int) *Dedup {
	return &Dedup{seen: map[string]time.Time{}, max: max}
}

// FirstSeen records id and reports whether it was new.
func (d *Dedup) FirstSeen(id string, now time.Time) bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	for k, t := range d.seen {
		if now.Sub(t) > 2*Tolerance {
			delete(d.seen, k)
		}
	}
	if _, ok := d.seen[id]; ok {
		return false
	}
	if len(d.seen) >= d.max {
		// Full: evict the oldest rather than refuse traffic.
		var oldest string
		var at time.Time
		for k, t := range d.seen {
			if oldest == "" || t.Before(at) {
				oldest, at = k, t
			}
		}
		delete(d.seen, oldest)
	}
	d.seen[id] = now
	return true
}

// Forget drops id, so a sender's retry is processed again. Called when
// processing failed after FirstSeen accepted the id.
func (d *Dedup) Forget(id string) {
	d.mu.Lock()
	defer d.mu.Unlock()
	delete(d.seen, id)
}
