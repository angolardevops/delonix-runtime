# __NAME__ (Go __TEMPLATE_VERSION__)

A Go HTTP service scaffolded by `delonix init -t go -v __TEMPLATE_VERSION__` —
stdlib-only (no framework, so `-v` pins the **toolchain**, not a package: both
`go.mod`'s `go __TEMPLATE_VERSION__` and the Delonixfile's
`FROM golang:__TEMPLATE_VERSION__-alpine`), Go modules, standard layout
(`cmd/`, `internal/`), the 1.22+ method+pattern `ServeMux`, tests with
`net/http/httptest`, a single-stage `Delonixfile`, and a Delonix manifest.

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
go get <module>                  # add a dependency (updates go.mod/go.sum)
```

## Choosing a Go version

Regenerate with `-v <major.minor>` to build against another toolchain, e.g.
`delonix init -t go -v 1.22 --force .` (`--force`: `init` never overwrites
without it). Omit `-v` and it falls back to this template's own default
(`__TEMPLATE_VERSION__`, from `template.meta`).

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```

## Contributing

CI (`.github/workflows/ci.yml` for GitHub, `.gitlab-ci.yml` for GitLab) runs
`go build`/`go vet`/`go test` on every push and PR. `sonar-project.properties`
is ready for a SonarQube/SonarCloud scan. See [CONTRIBUTING.md](CONTRIBUTING.md)
for the branching model (git-flow), commit message format (Conventional
Commits) and versioning (SemVer).
