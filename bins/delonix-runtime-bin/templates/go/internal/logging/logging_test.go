package logging

import (
	"bytes"
	"context"
	"encoding/json"
	"testing"

	"go.opentelemetry.io/otel/trace"
)

func TestSecretsAreRedactedAndTheTraceIsAttached(t *testing.T) {
	var buf bytes.Buffer
	log := New(&buf, "info", "svc", "1.0.0", "test")
	tid, _ := trace.TraceIDFromHex("4bf92f3577b34da6a3ce929d0e0e4736")
	sid, _ := trace.SpanIDFromHex("00f067aa0ba902b7")
	ctx := trace.ContextWithSpanContext(context.Background(), trace.NewSpanContext(trace.SpanContextConfig{
		TraceID: tid, SpanID: sid, TraceFlags: trace.FlagsSampled,
	}))
	log.InfoContext(ctx, "hello", "Authorization", "Bearer abc", "webhook_secret", "s3cr3t", "user", "ana")

	var line map[string]any
	if err := json.Unmarshal(buf.Bytes(), &line); err != nil {
		t.Fatalf("not one JSON line: %q", buf.String())
	}
	if line["Authorization"] != Redacted || line["webhook_secret"] != Redacted {
		t.Fatalf("a secret reached the log: %v", line)
	}
	if line["user"] != "ana" || line["service"] != "svc" || line["environment"] != "test" {
		t.Fatalf("missing fields: %v", line)
	}
	if line["trace_id"] != tid.String() || line["span_id"] != sid.String() {
		t.Fatalf("trace correlation missing: %v", line)
	}
	if bytes.Contains(buf.Bytes(), []byte("s3cr3t")) || bytes.Contains(buf.Bytes(), []byte("abc")) {
		t.Fatalf("secret bytes in output: %s", buf.String())
	}
}
