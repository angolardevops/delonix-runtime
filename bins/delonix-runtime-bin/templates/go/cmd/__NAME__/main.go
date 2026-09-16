// Command __NAME__ starts the HTTP server.
package main

import (
	"log"
	"net/http"
	"os"

	"__NAME__/internal/server"
)

func main() {
	addr := ":" + envOr("PORT", "__PORT__")
	log.Printf("listening on %s", addr)
	if err := http.ListenAndServe(addr, server.New()); err != nil {
		log.Fatal(err)
	}
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}
