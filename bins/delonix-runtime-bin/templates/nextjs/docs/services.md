# Services and versions

The images `compose.yaml` pins, why those versions, and how to move them.
Pinned, not floating — reproducibility comes from the tags here plus
`pnpm-lock.yaml` for the Node side, not from `latest`.

| Service | Image (pinned) | Role | Profile | State |
|---|---|---|---|---|
| app | built from `Dockerfile` (node:24-slim) | the Next.js service | default | validated |
| redis | `redis:7.4-alpine` | read-through cache (`lib/server/cache`) | default | upstream-available |
| rabbitmq | `rabbitmq:4.0-management-alpine` | `note.created` pub/sub (`lib/server/messaging`) | default | upstream-available |
| sonarqube | `sonarqube:lts-community` | code quality | quality | upstream-available |
| sonar-db | `postgres:16-alpine` | SonarQube's database | quality | upstream-available |

**State** — *validated*: generated and exercised by Delonix' test suite (the
project scaffolds, installs, and `pnpm test` passes). *upstream-available*: the
pinned tag exists upstream and the field has been wired, but this environment
has no container runtime, so `compose up` and a real Sonar scan were **not**
executed here. Run them on a host with Docker/Podman before relying on them.

Validated on: 2026-10-22.

## The app image

`Dockerfile` is a two-stage build that honours Next's standalone output
(`output: "standalone"` in `next.config.ts`): stage one runs `next build` and
`scripts/copy-static.mjs`, stage two carries only `.next/standalone` —
`server.js`, the traced production `node_modules`, and the browser assets.
`docker/entrypoint.sh` keeps `node` as PID 1 so SIGTERM reaches the ordered
shutdown (`docs/adr/0003-shutdown.md`). It mirrors the `Delonixfile` used by
`delonix build`; keep the two in step.

## Cache and messaging

- **Redis** — `lib/server/cache` has a `Cache` port with two adapters:
  `MemoryCache` (the default, and the graceful fallback when `REDIS_URL` is
  unset) and `RedisCache` (ioredis, every backend error swallowed so an outage
  is a slow path, never a failed request). `CachedNotes` wraps the use cases
  cache-aside. Wire it in `lib/server/runtime.ts` when `REDIS_URL` is set.
- **RabbitMQ** — `lib/server/messaging` has the `note.created` event
  (`events.ts`), a non-blocking bounded publisher that drops when full
  (`publisher.ts`, a `NotePublisher`; `CompositePublisher` fans out to it AND
  the webhook dispatcher), and a consumer whose ack / retry / dead-letter
  decision (`consumer.ts`) is unit-tested with no broker. The amqplib
  connect-and-consume glue is the developer's to add and the CI integration run
  exercises it against the compose `rabbitmq` service.

## Moving a version

- **Node / app deps** — `package.json` pins `next` to the template version;
  `ioredis` and `amqplib` are pinned with a floor and frozen by
  `pnpm-lock.yaml`. `pnpm update` takes a patch; a minor is a deliberate edit.
- **Service images** — edit the tag in `compose.yaml`, bring the stack up on a
  real engine, run `pnpm check` and a Sonar scan, then commit. Prefer pinning to
  a digest (`image: redis@sha256:…`) once you have validated one.

## Known prerequisites and limits

- **SonarQube** needs `vm.max_map_count=262144` on the host (Elasticsearch).
  `compose.yaml` does not change host sysctls; set it yourself (see README).
- **SonarQube community edition** analyses the main branch only — no
  pull-request or branch analysis (needs a paid edition).
- **`sonarqube:lts-community`** is a moving LTA tag; pin it to the exact version
  or a digest after your first validated scan.
- **Redis** runs without a password (internal Compose network, not published).
  Add `--requirepass` and a password in `REDIS_URL` before exposing it.
