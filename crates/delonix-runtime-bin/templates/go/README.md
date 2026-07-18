# __NAME__

A Go HTTP service scaffolded by `delonix ... init --template go` — stdlib-only
(no framework), Go modules, standard layout (`cmd/`, `internal/`), the Go 1.22+
method+pattern `ServeMux`, tests with `net/http/httptest`, a single-stage
`Delonixfile`, and a Delonix manifest.

## Layout

```
go.mod                       module + Go version
cmd/__NAME__/main.go         entrypoint (ListenAndServe)
internal/server/server.go    routes (ServeMux)
internal/server/health.go    /health/live + /health/ready
internal/server/server_test.go
Delonixfile                  static build, HEALTHCHECK
delonix-manifest.yaml        declarative deploy (kind: Container)
```

## Run it locally

```bash
go run ./cmd/__NAME__
curl localhost:__PORT__/api/v1/health/live
go test ./...
go build -o server ./cmd/__NAME__ && ./server
```

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```
