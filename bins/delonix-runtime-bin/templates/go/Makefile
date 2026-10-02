# Every command the README, CI and AGENTS.md mention is a target here.
BIN := server
VERSION ?= dev

.PHONY: fmt fmt-check vet test race cover vuln build run smoke check

fmt:        ## rewrite files with gofmt
	gofmt -w .
fmt-check:  ## fail if a file is not gofmt-formatted
	@test -z "$$(gofmt -l .)" || { gofmt -l .; exit 1; }
vet:
	go vet ./...
test:       ## unit, integration, contract (OpenAPI drift) and architecture tests
	go test ./...
race:
	go test -race -count=1 ./...
cover:
	go test -coverprofile=coverage.out ./...
vuln:       ## known vulnerabilities in the code paths actually used
	go run golang.org/x/vuln/cmd/govulncheck@v1.8.0 ./...
build:
	CGO_ENABLED=0 go build -trimpath -ldflags "-s -w -X main.version=$(VERSION)" -o $(BIN) ./cmd/__NAME__
run: build
	./$(BIN)
smoke:      ## against a running instance: make smoke URL=http://localhost:__PORT__
	sh ./scripts/smoke.sh $(or $(URL),http://localhost:__PORT__)
check: fmt-check vet race build  ## what CI runs
