package webhook

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"

	"__NAME__/internal/notes"
)

func dispatcherTo(t *testing.T, url string, attempts int) *Dispatcher {
	t.Helper()
	d := NewDispatcher(DispatcherConfig{
		URL: url, Key: make([]byte, 32), Client: http.DefaultClient,
		Timeout: time.Second, MaxAttempts: attempts, BaseBackoff: time.Millisecond, QueueSize: 8,
	}, slog.New(slog.NewJSONHandler(io.Discard, nil)))
	d.sleep = func(context.Context, time.Duration) error { return nil }
	return d
}

func TestRetriesServerErrorsThenDelivers(t *testing.T) {
	var calls atomic.Int32
	var ids []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ids = append(ids, r.Header.Get(HeaderID))
		if calls.Add(1) < 3 {
			w.WriteHeader(http.StatusServiceUnavailable)
			return
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	defer srv.Close()
	d := dispatcherTo(t, srv.URL, 4)
	d.NoteCreated(context.Background(), notes.Note{ID: "n1", Title: "t"})
	if err := d.Close(context.Background()); err != nil {
		t.Fatal(err)
	}
	if calls.Load() != 3 {
		t.Fatalf("want 3 attempts, got %d", calls.Load())
	}
	for _, id := range ids {
		if id != "n1" {
			t.Fatalf("every attempt must carry the same idempotency id, got %v", ids)
		}
	}
}

func TestAClientErrorIsNotRetried(t *testing.T) {
	var calls atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		calls.Add(1)
		w.WriteHeader(http.StatusBadRequest)
	}))
	defer srv.Close()
	d := dispatcherTo(t, srv.URL, 4)
	d.NoteCreated(context.Background(), notes.Note{ID: "n1"})
	_ = d.Close(context.Background())
	if calls.Load() != 1 {
		t.Fatalf("a 400 was retried %d times", calls.Load())
	}
}

func TestCloseIsBoundedAndLaterEventsAreDropped(t *testing.T) {
	block := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		<-block
		w.WriteHeader(http.StatusNoContent)
	}))
	defer srv.Close()
	defer close(block)
	d := dispatcherTo(t, srv.URL, 1)
	d.NoteCreated(context.Background(), notes.Note{ID: "slow"})
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	start := time.Now()
	if err := d.Close(ctx); err == nil {
		t.Fatal("Close reported a drained queue while a delivery was stuck")
	}
	if time.Since(start) > time.Second {
		t.Fatal("Close did not respect its deadline")
	}
	d.NoteCreated(context.Background(), notes.Note{ID: "late"}) // must not panic
}
