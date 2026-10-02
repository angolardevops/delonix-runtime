// Package archtest holds the dependency rules as a test, so `go test ./...`
// fails the moment a package reaches in the wrong direction.
package archtest

import (
	"go/build"
	"strings"
	"testing"
)

// rules: package directory → import prefixes it must never use.
var rules = map[string][]string{
	// The use cases know their two ports and the standard library — not the
	// transport, not telemetry, not a storage technology.
	"../notes": {
		"net/http",
		"go.opentelemetry.io/",
		"__NAME__/internal/httpapi",
		"__NAME__/internal/memstore",
		"__NAME__/internal/webhook",
		"__NAME__/internal/telemetry",
		"__NAME__/internal/config",
	},
	// Adapters implement ports; they do not call the transport.
	"../memstore": {"net/http", "__NAME__/internal/httpapi"},
	"../webhook":  {"__NAME__/internal/httpapi", "__NAME__/internal/memstore"},
	// The transport reaches the use cases through its Notes interface, never
	// a storage adapter directly.
	"../httpapi": {"__NAME__/internal/memstore"},
	"../config":  {"__NAME__/internal/"},
}

func TestDependencyDirection(t *testing.T) {
	for dir, forbidden := range rules {
		pkg, err := build.ImportDir(dir, 0)
		if err != nil {
			t.Fatalf("%s: %v", dir, err)
		}
		for _, imp := range pkg.Imports {
			for _, f := range forbidden {
				if strings.HasPrefix(imp, f) {
					t.Errorf("%s imports %s — forbidden by the dependency rules in ARCHITECTURE.md", dir, imp)
				}
			}
		}
	}
}
