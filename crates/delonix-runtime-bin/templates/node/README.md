# __NAME__

A Fastify + TypeScript service scaffolded by `delonix ... init --template node` —
a complete, best-practices starting point: **pnpm**-managed, ESM + strict TS,
health probes, tests (node:test via tsx), a single-stage `Delonixfile`, and a
Delonix manifest.

## Layout

```
package.json             pnpm project (scripts, packageManager pin)
tsconfig.json            strict, NodeNext ESM, out → dist/
.node-version            the Node major to use
Delonixfile              corepack pnpm build, HEALTHCHECK
delonix-manifest.yaml    declarative deploy (kind: Container)
src/index.ts             entrypoint (listen)
src/app.ts               application factory (Fastify)
src/routes/health.ts     /health/live + /health/ready
test/health.test.ts      node:test smoke tests
```

## Run it locally (pnpm)

```bash
corepack enable          # once — activates the pinned pnpm
pnpm install
pnpm dev                 # tsx watch (hot reload)
curl localhost:__PORT__/api/v1/health/live
pnpm test
pnpm build && pnpm start # production build + run
```

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .     # build the image from the Delonixfile
delonix stack apply                 # bring it up (kind: Container)
curl localhost:__PORT__/api/v1/health/live
```
