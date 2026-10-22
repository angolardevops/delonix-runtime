# Services and versions

The images `compose.yaml` pins, why those versions, and how to move them.
Pinned, not floating — reproducibility comes from the tags here plus
`composer.lock` for the PHP side, not from `latest`.

| Service | Image (pinned) | Role | Profile | State |
|---|---|---|---|---|
| app | built from `Dockerfile` (FrankenPHP 1, PHP 8.4) | the Laravel HTTP service | default | validated |
| app-worker | same image, `worker` command | delivers queued `note.created` webhooks (`queue:work`) | default | validated |
| redis | `redis:7.4-alpine` | cache store (`CACHE_STORE=redis`) | default | upstream-available |
| rabbitmq | `rabbitmq:4.0-management-alpine` | queue broker (`QUEUE_CONNECTION=rabbitmq`) | default | upstream-available |
| sonarqube | `sonarqube:lts-community` | code quality | quality | upstream-available |
| sonar-db | `postgres:16-alpine` | SonarQube's database | quality | upstream-available |

**State** — *validated*: generated and exercised by Delonix' test suite (the
template renders and its PHP tests pass). *upstream-available*: the pinned tag
exists upstream and the field has been wired, but this environment has no
container runtime, so `compose up` and a real Sonar scan were **not** executed
here. Run them on a host with Docker/Podman before relying on them.

Validated on: 2026-10-22.

## How the app reaches each service

- **Redis** — `config/cache.php` sets the default store from `CACHE_STORE`;
  `redis` uses the connection in `config/database.php` (framework default,
  reads `REDIS_URL`). The runtime image ships `ext-redis`. A bare
  `composer serve` leaves `CACHE_STORE=array` and needs no Redis.
- **RabbitMQ** — `config/queue.php` defines a `rabbitmq` connection
  (vladimir-yuldashev/laravel-queue-rabbitmq); `QUEUE_CONNECTION=rabbitmq`
  routes `App\Jobs\DeliverWebhook` through the broker, and the `app-worker`
  service runs `php artisan queue:work`. `database` keeps the jobs in SQLite
  with no broker.

## Moving a version

- **PHP / app deps** — `composer.json` pins `laravel/framework` and the
  queue/OTel packages with a floor; `composer.lock` freezes the exact
  resolution. `composer update <pkg>` takes one, a framework major is a
  deliberate edit.
- **Service images** — edit the tag in `compose.yaml`, bring the stack up on a
  real engine, run `composer check` and a Sonar scan, then commit. Prefer
  pinning to a digest (`image: redis@sha256:…`) once you have validated one.

## Known prerequisites and limits

- **SonarQube** needs `vm.max_map_count=262144` on the host (Elasticsearch).
  `compose.yaml` does not change host sysctls; set it yourself (see README).
- **SonarQube community edition** analyses the main branch only — no
  pull-request or branch analysis (needs a paid edition).
- **`sonarqube:lts-community`** is a moving LTA tag; pin it to the exact version
  or a digest after your first validated scan.
- **Redis** runs without a password (internal Compose network, not published).
  Add `--requirepass` and a password in `REDIS_URL` before exposing it.
