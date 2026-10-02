# 0004 — CommonJS output, Jest, TypeScript 6 — for NestJS 11 and 12 alike

Status: accepted (template default)

## Context
The template is generated for NestJS 11 or 12. NestJS 11 packages are
CommonJS; NestJS 12 packages are ES modules only. The NestJS 12 migration
guide states that a CommonJS application can stay CommonJS, loading the
packages through Node's `require(esm)`, and that Jest can load them on
Node 24.9 or later. `@nestjs/cli` 12 builds with TypeScript `~6.0.2`.

## Decision
One project layout for both majors: TypeScript `~6.0.2` with
`module: nodenext` and no `"type": "module"` (so files compile to CommonJS),
Node 24.9+, Jest 30 with `ts-jest`, started with `--experimental-vm-modules`
(Jest's `require(esm)` support needs the VM modules API). The telemetry entry
is loaded with `node --require`, which a CommonJS build allows.

## Alternatives
- An ES module project with Vitest (what `nest new` offers for ESM): Vitest
  needs an SWC plugin to emit the decorator metadata Nest's DI reads, and
  NestJS 11 would need a different setup — two layouts for one template.
- Type-stripping instead of a build: decorators with metadata are not
  supported by Node's type stripping.

## Trade-off
Tests print nothing about it, but they depend on an experimental Node flag
until Jest no longer needs it. Moving to ES modules later is a `tsconfig` and
`package.json` change plus `.js` extensions in relative imports.
