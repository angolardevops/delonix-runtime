package httpapi_test

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/propagation"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"

	"__NAME__/internal/health"
	"__NAME__/internal/httpapi"
	"__NAME__/internal/logging"
	"__NAME__/internal/memstore"
	"__NAME__/internal/notes"
	"__NAME__/internal/telemetry"
	"__NAME__/internal/webhook"
)

// syncBuffer is a log sink the dispatcher goroutine and the test can share.
type syncBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.Write(p)
}

func (s *syncBuffer) String() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.String()
}

// One request that creates a note must leave ONE trace across the incoming
// request, the use case, the outbound webhook call and the log lines — that
// is what makes a failure findable from any of the three.
func TestOneTraceAcrossRequestUseCaseOutboundCallAndLogs(t *testing.T) {
	exp := tracetest.NewInMemoryExporter()
	tp := sdktrace.NewTracerProvider(sdktrace.WithSyncer(exp))
	otel.SetTracerProvider(tp)
	otel.SetTextMapPropagator(propagation.TraceContext{})
	t.Cleanup(func() { _ = tp.Shutdown(context.Background()) })

	gotTraceparent := make(chan string, 1)
	gotSignature := make(chan bool, 1)
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body := new(bytes.Buffer)
		_, _ = body.ReadFrom(r.Body)
		err := webhook.Verify(testKey, r.Header.Get(webhook.HeaderID), r.Header.Get(webhook.HeaderTimestamp),
			r.Header.Get(webhook.HeaderSignature), body.Bytes(), time.Now())
		gotSignature <- err == nil
		gotTraceparent <- r.Header.Get("traceparent")
		w.WriteHeader(http.StatusNoContent)
	}))
	defer target.Close()

	logs := &syncBuffer{}
	log := logging.New(logs, "info", "svc", "test", "test")
	disp := webhook.NewDispatcher(webhook.DispatcherConfig{
		URL: target.URL, Key: testKey,
		Client:  &http.Client{Transport: otelhttp.NewTransport(http.DefaultTransport)},
		Timeout: 2 * time.Second, MaxAttempts: 2, BaseBackoff: 10 * time.Millisecond, QueueSize: 4,
	}, log)
	ready := health.New(nil)
	ready.SetReady()
	srv := httptest.NewServer(httpapi.New(httpapi.Deps{
		Notes:        telemetry.TracedNotes{Next: notes.NewService(memstore.New(), disp)},
		Readiness:    ready,
		Log:          log,
		MaxBodyBytes: 1 << 20,
		Dedup:        webhook.NewDedup(4),
	}))
	defer srv.Close()

	resp, err := http.Post(srv.URL+"/api/v1/notes", "application/json", strings.NewReader(`{"title":"traced"}`))
	if err != nil || resp.StatusCode != 201 {
		t.Fatalf("create: %v %v", resp, err)
	}
	resp.Body.Close()

	var traceparent string
	select {
	case traceparent = <-gotTraceparent:
	case <-time.After(5 * time.Second):
		t.Fatal("the outbound webhook never arrived")
	}
	if !<-gotSignature {
		t.Fatal("the outbound webhook signature did not verify")
	}
	if err := disp.Close(context.Background()); err != nil {
		t.Fatal(err)
	}

	var serverTrace string
	names := map[string]string{}
	for _, s := range exp.GetSpans() {
		names[s.Name] = s.SpanContext.TraceID().String()
		if s.Name == "POST /api/v1/notes" {
			serverTrace = s.SpanContext.TraceID().String()
		}
	}
	if serverTrace == "" {
		t.Fatalf("no server span; spans: %v", names)
	}
	for _, want := range []string{"notes.Create", "webhook.deliver"} {
		if names[want] != serverTrace {
			t.Errorf("span %q is in trace %q, want %q (spans: %v)", want, names[want], serverTrace, names)
		}
	}
	// traceparent: 00-<trace id>-<span id>-<flags>
	if !strings.Contains(traceparent, serverTrace) {
		t.Errorf("outbound traceparent %q does not carry trace %s", traceparent, serverTrace)
	}
	found := map[string]bool{}
	for _, line := range strings.Split(strings.TrimSpace(logs.String()), "\n") {
		var rec map[string]any
		if json.Unmarshal([]byte(line), &rec) == nil && rec["trace_id"] == serverTrace {
			found[rec["msg"].(string)] = true
		}
	}
	if !found["request"] || !found["webhook delivered"] {
		t.Errorf("log lines with trace %s: %v\n%s", serverTrace, found, logs.String())
	}
}
