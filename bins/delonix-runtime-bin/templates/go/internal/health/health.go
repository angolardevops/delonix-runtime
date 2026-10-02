// Package health tracks whether the process should receive traffic.
//
// Liveness only says the process answers. Readiness moves through three
// states: starting (not listening yet), ready, and draining (a shutdown
// signal arrived — finish what is in flight, take nothing new). A load
// balancer that polls readiness stops routing here before the listener
// closes.
package health

import (
	"context"
	"sync/atomic"
)

// State is the readiness state.
type State int32

const (
	Starting State = iota
	Ready
	Draining
)

func (s State) String() string {
	switch s {
	case Ready:
		return "ready"
	case Draining:
		return "draining"
	default:
		return "starting"
	}
}

// Check is a dependency the service cannot serve without. The example has
// none: the in-memory store cannot be down. Add one when a real database or
// broker is configured — never a check for something that is not used.
type Check func(ctx context.Context) error

// Readiness is safe for concurrent use.
type Readiness struct {
	state  atomic.Int32
	checks map[string]Check
}

// New starts in Starting.
func New(checks map[string]Check) *Readiness {
	return &Readiness{checks: checks}
}

// SetReady marks the service as able to take traffic.
func (r *Readiness) SetReady() { r.state.CompareAndSwap(int32(Starting), int32(Ready)) }

// SetDraining is one-way: nothing brings a draining process back.
func (r *Readiness) SetDraining() { r.state.Store(int32(Draining)) }

// State is the current state.
func (r *Readiness) State() State { return State(r.state.Load()) }

// Evaluate returns the state and, when ready, the name of the first failing
// dependency (empty if all pass). Failure details stay in the logs, never in
// the response.
func (r *Readiness) Evaluate(ctx context.Context) (State, string) {
	st := r.State()
	if st != Ready {
		return st, ""
	}
	for name, check := range r.checks {
		if err := check(ctx); err != nil {
			return st, name
		}
	}
	return st, ""
}
