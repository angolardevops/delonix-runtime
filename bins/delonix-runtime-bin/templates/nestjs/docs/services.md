# Services and versions

The images `compose.yaml` pins, why those versions, and how to move them.
Pinned, not floating — reproducibility comes from the tags here plus
`pnpm-lock.yaml` for the Node side, not from `latest`.

| Service | Image (pinned) | Role | Profile | State |
|---|---|---|---|---|
| app | built from `Dockerfile` (node:24-slim) | the NestJS service | default | validated |
| redis | `redis:7.4-alpine` | read-through cache | default | upstream-available |
| rabbitmq | `rabbitmq:4.0-management-alpine` | `note.created` pub/sub | default | upstream-available |
| sonarqube | `sonarqube:lts-community` | code quality | quality | upstream-available |
| sonar-db | `postgres:16-alpine` | SonarQube's database | quality | upstream-available |

**State** — *validated*: generated and exercised by Delonix' test suite (the
project installs and its tests pass). *upstream-available*: the pinned tag
exists upstream and the field has been wired, but this environment has no
container runtime, so `compose up` and a real Sonar scan were **not** executed
here. Run them on a host with Docker/Podman before relying on them.

Validated on: 2026-10-22.

## How the app uses each service

- **Redis** backs the cache-aside layer (`src/cache`). Set `REDIS_URL` and
  `get(id)` reads through Redis; leave it unset and the in-process
  `InMemoryCache` is used instead. Every backend error is swallowed, so a down
  Redis turns the cache off (reads fall through to the store), never a failed
  request.
- **RabbitMQ** carries `note.created` (`src/messaging`). Set `RABBITMQ_URL` and
  the `RabbitPublisher` joins the outbound webhook dispatcher behind one
  `NotePublisher` (`CompositePublisher`); publishing is non-blocking and bounded,
  and drops rather than blocks a request when the queue is full.

## Moving a version

- **Node / app deps** — `package.json` pins `@nestjs/*` and the runtime
  libraries; `pnpm up` takes a patch, a minor is a deliberate edit. `ioredis`
  and `amqplib` are pinned with a floor and frozen by `pnpm-lock.yaml`.
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
