# __NAME__ (Next.js)

A Next.js (App Router) app scaffolded by `delonix init --template nextjs` —
TypeScript, React 19, Route Handlers for health, **pnpm**, a Delonix manifest.

## Run it locally (pnpm)

```bash
corepack enable
pnpm install
pnpm dev
curl localhost:__PORT__/api/v1/health/live
```

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```
