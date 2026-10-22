// Package cache is the read-through cache. It names what the cache-aside
// wrapper needs from a backend as one small interface (Cache), with two
// adapters: an in-process map (the default, and the graceful fallback when no
// REDIS_URL is set) and Redis. Every Cache method is total — a backend failure
// is swallowed (a miss on Get, a no-op on Set), so a cache outage is a slow
// path, never a failed request. It wraps the use cases from the OUTSIDE, so the
// notes package never imports it (the architecture test enforces that).
package cache

import (
	"context"
	"sync"
	"time"
)

// Cache is a bytes key/value store with per-entry expiry. Failures are
// swallowed: Get reports a miss, Set does nothing.
type Cache interface {
	// Get returns the stored value and true, or nil and false on a miss OR any
	// backend error.
	Get(ctx context.Context, key string) ([]byte, bool)
	// Set stores value under key for ttl. A no-op when ttl <= 0 or on error.
	Set(ctx context.Context, key string, value []byte, ttl time.Duration)
	// Healthy reports whether the backend answers (for the readiness probe).
	Healthy(ctx context.Context) bool
	// Close releases connections. Called once on shutdown.
	Close() error
}

// InMemory is the in-process Cache. The default when no REDIS_URL is set.
//
// Entries live in a map with a monotonic-clock expiry; a bounded size evicts
// the oldest first so a long run cannot grow without limit. Lost on restart and
// not shared between replicas — for local runs, tests, and as the fallback when
// Redis is not configured. Safe for concurrent use.
type InMemory struct {
	mu    sync.Mutex
	max   int
	clock func() time.Time
	byKey map[string]entry
	order []string // oldest first
}

type entry struct {
	expiresAt time.Time
	value     []byte
}

// InMemoryOption tunes an InMemory cache.
type InMemoryOption func(*InMemory)

// WithMaxEntries bounds how many entries the cache keeps (default 4096).
func WithMaxEntries(n int) InMemoryOption {
	return func(c *InMemory) {
		if n > 0 {
			c.max = n
		}
	}
}

// WithClock injects the clock (for tests). Defaults to time.Now.
func WithClock(clock func() time.Time) InMemoryOption {
	return func(c *InMemory) {
		if clock != nil {
			c.clock = clock
		}
	}
}

// NewInMemory returns an empty in-process cache.
func NewInMemory(opts ...InMemoryOption) *InMemory {
	c := &InMemory{max: 4096, clock: time.Now, byKey: map[string]entry{}}
	for _, o := range opts {
		o(c)
	}
	return c
}

func (c *InMemory) Get(_ context.Context, key string) ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	e, ok := c.byKey[key]
	if !ok {
		return nil, false
	}
	if !c.clock().Before(e.expiresAt) {
		c.delete(key)
		return nil, false
	}
	return e.value, true
}

func (c *InMemory) Set(_ context.Context, key string, value []byte, ttl time.Duration) {
	if ttl <= 0 {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if _, ok := c.byKey[key]; ok {
		c.removeOrder(key) // re-insert as newest
	} else if len(c.byKey) >= c.max {
		c.delete(c.order[0]) // full: evict the oldest
	}
	c.byKey[key] = entry{expiresAt: c.clock().Add(ttl), value: value}
	c.order = append(c.order, key)
}

// Healthy: the in-process cache always answers.
func (c *InMemory) Healthy(context.Context) bool { return true }

// Close clears the cache.
func (c *InMemory) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.byKey = map[string]entry{}
	c.order = nil
	return nil
}

// delete removes a key from both the map and the order slice. Caller holds mu.
func (c *InMemory) delete(key string) {
	delete(c.byKey, key)
	c.removeOrder(key)
}

func (c *InMemory) removeOrder(key string) {
	for i, k := range c.order {
		if k == key {
			c.order = append(c.order[:i], c.order[i+1:]...)
			return
		}
	}
}

var _ Cache = (*InMemory)(nil)
