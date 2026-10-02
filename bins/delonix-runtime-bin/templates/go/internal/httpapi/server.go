// Package httpapi is the HTTP transport: routing, request decoding, error
// mapping and the cross-cutting middleware (request id, tracing, access log,
// panic recovery, body limit). It translates HTTP to use-case calls and back;
// no business rule lives here.
package httpapi

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"log/slog"
	"net/http"
	"regexp"
	"strings"
	"time"

	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"

	"__NAME__/internal/health"
	"__NAME__/internal/notes"
	"__NAME__/internal/webhook"
)

// Notes is what the transport needs from the use cases.
type Notes interface {
	Create(ctx context.Context, in notes.CreateInput) (notes.Note, error)
	Get(ctx context.Context, id string) (notes.Note, error)
	List(ctx context.Context, offset, limit int) (notes.Page, error)
}

// Deps wires the handler.
type Deps struct {
	Notes        Notes
	Readiness    *health.Readiness
	Log          *slog.Logger
	MaxBodyBytes int64
	// WebhookKey enables the inbound webhook endpoint; nil disables it.
	WebhookKey []byte
	Dedup      *webhook.Dedup
	Now        func() time.Time
}

// Routes is the route table. The OpenAPI contract (api/openapi.yaml) must
// list exactly these; TestOpenAPIMatchesRoutes fails on any drift.
func Routes(d Deps) []Route {
	h := &handlers{d: d}
	rs := []Route{
		{"GET", "/api/v1/health/live", h.live},
		{"GET", "/api/v1/health/ready", h.ready},
		{"POST", "/api/v1/notes", h.createNote},
		{"GET", "/api/v1/notes", h.listNotes},
		{"GET", "/api/v1/notes/{id}", h.getNote},
	}
	if d.WebhookKey != nil {
		rs = append(rs, Route{"POST", "/api/v1/webhooks/inbound", h.inboundWebhook})
	}
	return rs
}

// Route is one method+pattern and its handler.
type Route struct {
	Method, Pattern string
	Handler         http.HandlerFunc
}

// New returns the complete handler.
func New(d Deps) http.Handler {
	if d.Now == nil {
		d.Now = time.Now
	}
	mux := http.NewServeMux()
	for _, r := range Routes(d) {
		mux.Handle(r.Method+" "+r.Pattern, named(r.Pattern, r.Handler))
	}
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		writeError(w, r, http.StatusNotFound, "not_found", "no such route", nil)
	})
	traced := otelhttp.NewHandler(accessLog(d.Log, mux), "http.server",
		// Probes every few seconds would drown the traces that matter.
		otelhttp.WithFilter(func(r *http.Request) bool {
			return !strings.HasPrefix(r.URL.Path, "/api/v1/health/")
		}),
	)
	return requestID(recoverer(d.Log, bodyLimit(d.MaxBodyBytes, traced)))
}

// named renames the span to the matched pattern and labels the metrics with
// it: the pattern, not the raw path, so /notes/{id} is one series and not one
// per id.
func named(pattern string, h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		span := trace.SpanFromContext(r.Context())
		span.SetName(r.Method + " " + pattern)
		span.SetAttributes(attribute.String("http.route", pattern))
		if l, ok := otelhttp.LabelerFromContext(r.Context()); ok {
			l.Add(attribute.String("http.route", pattern))
		}
		h.ServeHTTP(w, r)
	})
}

type ctxKey struct{}

// RequestIDFrom returns the request id set by the middleware.
func RequestIDFrom(ctx context.Context) string {
	id, _ := ctx.Value(ctxKey{}).(string)
	return id
}

// A client-sent X-Request-Id is kept only if it is short and plain; anything
// else is replaced, so a header cannot inject content into the logs.
var validRequestID = regexp.MustCompile(`^[A-Za-z0-9._-]{1,64}$`)

func requestID(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := r.Header.Get("X-Request-Id")
		if !validRequestID.MatchString(id) {
			var b [8]byte
			_, _ = rand.Read(b[:])
			id = hex.EncodeToString(b[:])
		}
		w.Header().Set("X-Request-Id", id)
		next.ServeHTTP(w, r.WithContext(context.WithValue(r.Context(), ctxKey{}, id)))
	})
}

func bodyLimit(max int64, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		r.Body = http.MaxBytesReader(w, r.Body, max)
		next.ServeHTTP(w, r)
	})
}

type statusWriter struct {
	http.ResponseWriter
	status int
}

func (s *statusWriter) WriteHeader(code int) {
	s.status = code
	s.ResponseWriter.WriteHeader(code)
}

// accessLog runs inside the tracing handler, so its line carries the
// request's trace_id. It logs the path, never the query string or headers.
func accessLog(log *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		sw := &statusWriter{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(sw, r)
		lvl := slog.LevelInfo
		if strings.HasPrefix(r.URL.Path, "/api/v1/health/") {
			lvl = slog.LevelDebug
		}
		log.Log(r.Context(), lvl, "request",
			"method", r.Method,
			"path", r.URL.Path,
			"status", sw.status,
			"duration_ms", time.Since(start).Milliseconds(),
			"request_id", RequestIDFrom(r.Context()),
		)
	})
}

// recoverer turns a panic into a 500 without the stack in the response.
func recoverer(log *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if v := recover(); v != nil {
				if v == http.ErrAbortHandler {
					panic(v)
				}
				log.ErrorContext(r.Context(), "panic serving request", "panic", v, "path", r.URL.Path)
				writeError(w, r, http.StatusInternalServerError, "internal", "internal error", nil)
			}
		}()
		next.ServeHTTP(w, r)
	})
}
