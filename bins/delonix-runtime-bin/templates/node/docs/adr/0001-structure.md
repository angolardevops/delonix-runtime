# 0001 — Fastify plugins as modules, schemas as the contract

Status: accepted (template default)

## Context
The service needs routing, validation, an HTTP contract and a structure that
survives a second capability. Fastify already has encapsulated plugins, JSON
schema validation and a logger; adding a framework on top would duplicate them.

## Decision
One plugin per concern, registered in `src/app.ts`. Route schemas are the
single source of the HTTP contract: `@fastify/swagger` generates OpenAPI from
them and `api/openapi.json` is the checked-in copy, kept honest by a test.
Business rules live in `src/notes/notes.ts` behind two ports declared there.
Tests use `node:test` (ships with Node, no runner to configure) and
`app.inject` (no socket). A second transport (gRPC) is not generated: nothing
needs it yet, and it would be a second module over the same use cases.

## Alternatives
- A hand-written OpenAPI file as the source of truth: two descriptions of every
  route to keep in step by hand.
- TypeBox/Zod type providers: worth adopting when schemas grow; plain JSON
  Schema keeps the template free of one more dependency.
- A DI container: Fastify options and a composition root are enough at this size.

## Trade-off
The schema checks shape and types while the use case checks the rules, so a
limit appears in two places (imported from one constant).
