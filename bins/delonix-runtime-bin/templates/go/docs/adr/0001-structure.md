# 0001 — Standard library HTTP, ports declared by their consumer

Status: accepted (template default)

## Context
The service needs routing, JSON, timeouts and graceful shutdown. Go 1.22+
`net/http` has method+pattern routing; a framework adds a dependency and its
own idioms for the same four things.

## Decision
Use `net/http` directly. Structure by responsibility (`notes`, `httpapi`,
adapters) with interfaces declared where they are consumed. Dependency
direction is a test (`internal/archtest`), not a convention.

## Alternatives
- A router/framework (chi, echo, gin): worth it for middleware ecosystems or
  many routes; not needed for this size.
- Layer-per-directory (`controllers/`, `services/`, `repositories/`): names
  layers instead of capabilities and invites an interface per class.

## Trade-off
Less out-of-the-box middleware; each cross-cutting concern here is a few
lines in `internal/httpapi/server.go`.
