# 0001 — Module per domain, ports as abstract classes, Express adapter

Status: accepted (template default)

## Context
NestJS brings routing, dependency injection, pipes, filters and interceptors.
The service needs one capability whose rules stay testable without HTTP, and
adapters (storage, outbound webhook) that can be replaced.

## Decision
One Nest module per domain (`NotesModule`). The use cases are an injectable
service; what they need from outside is an abstract class (`NoteRepository`,
`NotePublisher`) — in Nest the class is both the contract and the injection
token, so no string tokens or `@Inject()` are needed. Modules bind each port
to an adapter. The dependency direction is a test (`src/architecture.spec.ts`),
not a convention. The HTTP platform is Express, Nest's default.

## Alternatives
- Fastify adapter: faster on paper; it changes the raw-body, middleware and
  instrumentation story, and nothing in this service is bound by the platform.
- Interfaces + `Symbol` tokens for ports: closer to textbook ports and
  adapters, more ceremony (`@Inject(TOKEN)` at each use).
- A framework-free use-case layer wired with factory providers: no
  `@Injectable()` in the use case, one factory to maintain per use case.
- `@nestjs/config`: schema validation needs another library; one typed loader
  (`src/config/app-config.ts`) is less code than its integration.

## Trade-off
The use-case class imports `@nestjs/common` for `@Injectable()`. It is the
only framework import the architecture test allows there.
