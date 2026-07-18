package server

import (
	"encoding/json"
	"net/http"
)

// live is the liveness probe (used by the Delonixfile HEALTHCHECK).
func live(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, map[string]string{"status": "alive"})
}

// ready is the readiness probe (extend with DB/cache checks).
func ready(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, map[string]string{"status": "ready"})
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}
