package health

import (
	"context"
	"errors"
	"testing"
)

func TestTransitionsAreOrderedAndDrainingIsFinal(t *testing.T) {
	r := New(nil)
	if r.State() != Starting {
		t.Fatal("must start in Starting")
	}
	r.SetReady()
	if r.State() != Ready {
		t.Fatal("SetReady did not move to Ready")
	}
	r.SetDraining()
	r.SetReady()
	if r.State() != Draining {
		t.Fatal("a draining process came back to Ready")
	}
}

func TestAFailingDependencyIsNamed(t *testing.T) {
	r := New(map[string]Check{"db": func(context.Context) error { return errors.New("down") }})
	r.SetReady()
	if st, failing := r.Evaluate(context.Background()); st != Ready || failing != "db" {
		t.Fatalf("got %v %q", st, failing)
	}
}
