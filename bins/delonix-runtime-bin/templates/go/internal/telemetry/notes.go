package telemetry

import (
	"context"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"

	"__NAME__/internal/notes"
)

// TracedNotes wraps the use cases with one span each, so a trace shows the
// business step between the HTTP span and the calls it makes — without the
// notes package importing any telemetry.
type TracedNotes struct {
	Next *notes.Service
}

var tracer = otel.Tracer("__NAME__/notes")

func end(span trace.Span, err error) {
	if err != nil {
		span.RecordError(err)
		span.SetStatus(codes.Error, "use case failed")
	}
	span.End()
}

func (t TracedNotes) Create(ctx context.Context, in notes.CreateInput) (notes.Note, error) {
	ctx, span := tracer.Start(ctx, "notes.Create")
	n, err := t.Next.Create(ctx, in)
	end(span, err)
	return n, err
}

func (t TracedNotes) Get(ctx context.Context, id string) (notes.Note, error) {
	ctx, span := tracer.Start(ctx, "notes.Get")
	n, err := t.Next.Get(ctx, id)
	end(span, err)
	return n, err
}

func (t TracedNotes) List(ctx context.Context, offset, limit int) (notes.Page, error) {
	ctx, span := tracer.Start(ctx, "notes.List")
	p, err := t.Next.List(ctx, offset, limit)
	end(span, err)
	return p, err
}
