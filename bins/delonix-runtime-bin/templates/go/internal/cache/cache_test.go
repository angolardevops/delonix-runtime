package cache

import (
	"context"
	"errors"
	"testing"
	"time"

	"__NAME__/internal/memstore"
	"__NAME__/internal/notes"
)

var ctx = context.Background()

// ---- InMemory ----

func TestInMemoryRoundtripAndMiss(t *testing.T) {
	c := NewInMemory()
	if _, ok := c.Get(ctx, "absent"); ok {
		t.Fatal("absent key reported a hit")
	}
	c.Set(ctx, "k", []byte("v"), 10*time.Second)
	v, ok := c.Get(ctx, "k")
	if !ok || string(v) != "v" {
		t.Fatalf("got %q, %v; want v, true", v, ok)
	}
}

func TestInMemoryEntryExpiresAfterTTL(t *testing.T) {
	now := time.Unix(0, 0)
	c := NewInMemory(WithClock(func() time.Time { return now }))
	c.Set(ctx, "k", []byte("v"), 10*time.Second)
	now = time.Unix(0, 0).Add(9900 * time.Millisecond)
	if _, ok := c.Get(ctx, "k"); !ok {
		t.Fatal("entry expired early")
	}
	now = time.Unix(0, 0).Add(10 * time.Second)
	if _, ok := c.Get(ctx, "k"); ok {
		t.Fatal("entry did not expire at the TTL")
	}
}

func TestInMemoryEvictsOldestWhenFull(t *testing.T) {
	c := NewInMemory(WithMaxEntries(2))
	c.Set(ctx, "a", []byte("1"), 100*time.Second)
	c.Set(ctx, "b", []byte("2"), 100*time.Second)
	c.Set(ctx, "c", []byte("3"), 100*time.Second) // evicts "a"
	if _, ok := c.Get(ctx, "a"); ok {
		t.Fatal("oldest entry was not evicted")
	}
	if _, ok := c.Get(ctx, "b"); !ok {
		t.Fatal("b was evicted")
	}
	if _, ok := c.Get(ctx, "c"); !ok {
		t.Fatal("c was evicted")
	}
}

func TestInMemoryRejectsNonPositiveTTL(t *testing.T) {
	c := NewInMemory()
	c.Set(ctx, "k", []byte("v"), 0)
	if _, ok := c.Get(ctx, "k"); ok {
		t.Fatal("a non-positive TTL was stored")
	}
}

// ---- CachedNotes ----

// countingNotes counts how often the store is actually read.
type countingNotes struct {
	inner *notes.Service
	gets  int
}

func newCounting() *countingNotes {
	return &countingNotes{inner: notes.NewService(memstore.New(), nil)}
}

func (c *countingNotes) Create(ctx context.Context, in notes.CreateInput) (notes.Note, error) {
	return c.inner.Create(ctx, in)
}

func (c *countingNotes) Get(ctx context.Context, id string) (notes.Note, error) {
	c.gets++
	return c.inner.Get(ctx, id)
}

func (c *countingNotes) List(ctx context.Context, offset, limit int) (notes.Page, error) {
	return c.inner.List(ctx, offset, limit)
}

func TestCreatePrewarmsSoTheFirstGetIsAHit(t *testing.T) {
	inner := newCounting()
	cached := NewCachedNotes(inner, NewInMemory(), 100*time.Second, nil)
	note, err := cached.Create(ctx, notes.CreateInput{Title: "hello"})
	if err != nil {
		t.Fatal(err)
	}
	got, err := cached.Get(ctx, note.ID)
	if err != nil || got.ID != note.ID {
		t.Fatalf("got %+v, %v", got, err)
	}
	if inner.gets != 0 {
		t.Fatalf("store was read %d times; want 0 (served from the cache)", inner.gets)
	}
}

func TestSecondGetIsServedFromTheCache(t *testing.T) {
	inner := newCounting()
	cached := NewCachedNotes(inner, NewInMemory(), 100*time.Second, nil)
	created, err := inner.Create(ctx, notes.CreateInput{Title: "x"}) // cache stays cold
	if err != nil {
		t.Fatal(err)
	}
	if _, err := cached.Get(ctx, created.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := cached.Get(ctx, created.ID); err != nil {
		t.Fatal(err)
	}
	if inner.gets != 1 {
		t.Fatalf("store read %d times; want 1 (the second get was a hit)", inner.gets)
	}
}

func TestUnknownNoteStillRaisesThroughTheCache(t *testing.T) {
	cached := NewCachedNotes(newCounting(), NewInMemory(), 100*time.Second, nil)
	if _, err := cached.Get(ctx, "nope"); !errors.Is(err, notes.ErrNotFound) {
		t.Fatalf("got %v; want ErrNotFound", err)
	}
}

func TestACorruptCacheEntryIsIgnored(t *testing.T) {
	inner := newCounting()
	c := NewInMemory()
	created, err := inner.Create(ctx, notes.CreateInput{Title: "x"})
	if err != nil {
		t.Fatal(err)
	}
	c.Set(ctx, keyPrefix+created.ID, []byte("not json"), 100*time.Second)
	cached := NewCachedNotes(inner, c, 100*time.Second, nil)
	if got, err := cached.Get(ctx, created.ID); err != nil || got.ID != created.ID {
		t.Fatalf("got %+v, %v; want the stored note", got, err)
	}
	if inner.gets != 1 {
		t.Fatalf("store read %d times; want 1 (fell through the corrupt entry)", inner.gets)
	}
}

// brokenCache models the port contract under a backend outage: never errors,
// never stores.
type brokenCache struct{}

func (brokenCache) Get(context.Context, string) ([]byte, bool)         { return nil, false }
func (brokenCache) Set(context.Context, string, []byte, time.Duration) {}
func (brokenCache) Healthy(context.Context) bool                       { return false }
func (brokenCache) Close() error                                       { return nil }

func TestADeadCacheBackendStillServesFromTheStore(t *testing.T) {
	inner := newCounting()
	created, err := inner.Create(ctx, notes.CreateInput{Title: "x"})
	if err != nil {
		t.Fatal(err)
	}
	cached := NewCachedNotes(inner, brokenCache{}, 100*time.Second, nil)
	if _, err := cached.Get(ctx, created.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := cached.Get(ctx, created.ID); err != nil {
		t.Fatal(err)
	}
	if inner.gets != 2 {
		t.Fatalf("store read %d times; want 2 (every read falls through)", inner.gets)
	}
}

// ---- Redis adapter: errors are swallowed (no server needed) ----

func TestRedisCacheSwallowsBackendErrors(t *testing.T) {
	// Port 1 refuses connections at once: every call hits the backend-error path.
	c, err := NewRedis("redis://127.0.0.1:1/0", WithTimeout(200*time.Millisecond))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	if _, ok := c.Get(ctx, "k"); ok {
		t.Fatal("a down backend reported a hit")
	}
	c.Set(ctx, "k", []byte("v"), 10*time.Second) // must not panic or block
	if c.Healthy(ctx) {
		t.Fatal("a down backend reported healthy")
	}
}

func TestRedisRejectsABadURL(t *testing.T) {
	if _, err := NewRedis("not-a-redis-url"); err == nil {
		t.Fatal("a bad URL was accepted")
	}
}
