// Package server wires the HTTP routes.
package server

import "net/http"

// New returns the HTTP handler with all routes (Go 1.22+ method+pattern mux).
func New() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/v1/health/live", live)
	mux.HandleFunc("GET /api/v1/health/ready", ready)
	return mux
}
