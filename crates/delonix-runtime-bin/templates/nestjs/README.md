# __NAME__ (NestJS)

A NestJS service scaffolded by `delonix init --template nestjs` — TypeScript,
decorators, modular structure, **pnpm**, health probes, a Delonix manifest.

## Run it locally (pnpm)

```bash
corepack enable
pnpm install
pnpm start:dev
curl localhost:__PORT__/api/v1/health/live
```

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```
