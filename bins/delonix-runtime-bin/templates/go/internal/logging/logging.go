// Package logging builds the process logger: JSON lines on stdout, the
// service identity on every line, the trace and span ids of the current
// request when there is one, and secrets redacted by key before they reach
// the output.
package logging

import (
	"context"
	"io"
	"log/slog"
	"strings"

	"go.opentelemetry.io/otel/trace"
)

// sensitive key fragments. A value whose key contains one is replaced, so a
// careless `slog.String("authorization", h)` cannot leak a token.
var sensitive = []string{"authorization", "cookie", "password", "secret", "token", "signature", "api_key", "apikey"}

// Redacted is what a sensitive value becomes.
const Redacted = "[REDACTED]"

// New returns the logger. level is one of debug, info, warn, error.
func New(w io.Writer, level, service, version, env string) *slog.Logger {
	var lvl slog.Level
	_ = lvl.UnmarshalText([]byte(level))
	h := slog.NewJSONHandler(w, &slog.HandlerOptions{
		Level: lvl,
		ReplaceAttr: func(_ []string, a slog.Attr) slog.Attr {
			key := strings.ToLower(a.Key)
			for _, s := range sensitive {
				if strings.Contains(key, s) {
					return slog.String(a.Key, Redacted)
				}
			}
			return a
		},
	})
	return slog.New(traceHandler{h}).With(
		slog.String("service", service),
		slog.String("version", version),
		slog.String("environment", env),
	)
}

// traceHandler adds trace_id/span_id from the record's context.
type traceHandler struct{ slog.Handler }

func (h traceHandler) Handle(ctx context.Context, r slog.Record) error {
	if sc := trace.SpanContextFromContext(ctx); sc.IsValid() {
		r.AddAttrs(
			slog.String("trace_id", sc.TraceID().String()),
			slog.String("span_id", sc.SpanID().String()),
		)
	}
	return h.Handler.Handle(ctx, r)
}

func (h traceHandler) WithAttrs(as []slog.Attr) slog.Handler {
	return traceHandler{h.Handler.WithAttrs(as)}
}

func (h traceHandler) WithGroup(name string) slog.Handler {
	return traceHandler{h.Handler.WithGroup(name)}
}
