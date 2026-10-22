# Services and versions

The images `compose.yaml` pins, why those versions, and how to move them.
Pinned, not floating — reproducibility comes from the tags here plus `uv.lock`
for the Python side, not from `latest`.

| Service | Image (pinned) | Role | Profile | State |
|---|---|---|---|---|
| app | built from `Dockerfile` (python:3.13-slim) | the Django service (gunicorn) | default | validated |
| app-consumer | same image, `manage.py consume_events` | reads `note.created` from RabbitMQ | default | upstream-available |
| redis | `redis:7.4-alpine` | Django cache backend | default | upstream-available |
| rabbitmq | `rabbitmq:4.0-management-alpine` | `note.created` pub/sub | default | upstream-available |
| sonarqube | `sonarqube:lts-community` | code quality | quality | upstream-available |
| sonar-db | `postgres:16-alpine` | SonarQube's database | quality | upstream-available |

**State** — *validated*: generated and exercised by Delonix' test suite (the
template renders and its Python tests pass). *upstream-available*: the pinned
tag exists upstream and the field has been wired, but this environment has no
container runtime, so `compose up` and a real Sonar scan were **not** executed
here. Run them on a host with Docker/Podman before relying on them.

Validated on: 2026-10-22.

## Moving a version

- **Python / app deps** — `pyproject.toml` pins `django==X.Y.*`; `uv lock
  --upgrade` takes a patch, a minor is a deliberate edit. `django-redis` and
  `pika` are pinned with a floor and frozen by `uv.lock`.
- **Service images** — edit the tag in `compose.yaml`, bring the stack up on a
  real engine, run `make check`, then commit. Prefer pinning to a digest
  (`image: redis@sha256:…`) once you have validated one.

## Known prerequisites and limits

- **SonarQube** needs `vm.max_map_count=262144` on the host (Elasticsearch).
  `compose.yaml` does not change host sysctls; set it yourself (see README).
- **SonarQube community edition** analyses the main branch only — no
  pull-request or branch analysis (needs a paid edition).
- **`sonarqube:lts-community`** is a moving LTA tag; pin it to the exact version
  or a digest after your first validated scan.
- **Redis** runs without a password (internal Compose network, not published).
  The app degrades to the in-process cache if it is unreachable.
- **The app** keeps its SQLite database and `SECRET_KEY` on the `__NAME__-data`
  volume. The `__NAME__-consumer` shares that volume so it reads the same key;
  for several web replicas, move to Postgres and run migrations as a release
  step (set `MIGRATE_ON_START` off).
