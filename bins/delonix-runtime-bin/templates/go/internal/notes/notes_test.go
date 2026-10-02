package notes_test

import (
	"context"
	"errors"
	"strings"
	"testing"

	"__NAME__/internal/memstore"
	"__NAME__/internal/notes"
)

type recorder struct{ got []notes.Note }

func (r *recorder) NoteCreated(_ context.Context, n notes.Note) { r.got = append(r.got, n) }

func TestCreateStoresPublishesAndTrims(t *testing.T) {
	pub := &recorder{}
	svc := notes.NewService(memstore.New(), pub)
	n, err := svc.Create(context.Background(), notes.CreateInput{Title: "  hello  ", Body: "world"})
	if err != nil {
		t.Fatal(err)
	}
	if n.Title != "hello" || n.ID == "" {
		t.Fatalf("unexpected note %+v", n)
	}
	got, err := svc.Get(context.Background(), n.ID)
	if err != nil || got != n {
		t.Fatalf("stored note differs: %+v %v", got, err)
	}
	if len(pub.got) != 1 || pub.got[0].ID != n.ID {
		t.Fatalf("note was not published: %+v", pub.got)
	}
}

func TestCreateRejectsEveryInvalidFieldAndPublishesNothing(t *testing.T) {
	pub := &recorder{}
	svc := notes.NewService(memstore.New(), pub)
	_, err := svc.Create(context.Background(), notes.CreateInput{
		Title: " ",
		Body:  strings.Repeat("x", notes.MaxBodyLen+1),
	})
	var verr *notes.ValidationError
	if !errors.As(err, &verr) || verr.Fields["title"] == "" || verr.Fields["body"] == "" {
		t.Fatalf("want a validation error on title and body, got %v", err)
	}
	if len(pub.got) != 0 {
		t.Fatal("an invalid note was published")
	}
}

func TestListPagesAndBounds(t *testing.T) {
	svc := notes.NewService(memstore.New(), nil)
	for _, title := range []string{"a", "b", "c"} {
		if _, err := svc.Create(context.Background(), notes.CreateInput{Title: title}); err != nil {
			t.Fatal(err)
		}
	}
	p, err := svc.List(context.Background(), 1, 1)
	if err != nil || p.Total != 3 || len(p.Items) != 1 {
		t.Fatalf("page: %+v %v", p, err)
	}
	if _, err := svc.List(context.Background(), 0, notes.MaxPageSize+1); err == nil {
		t.Fatal("an oversized page was accepted")
	}
	if _, err := svc.Get(context.Background(), "missing"); !errors.Is(err, notes.ErrNotFound) {
		t.Fatalf("want ErrNotFound, got %v", err)
	}
}
