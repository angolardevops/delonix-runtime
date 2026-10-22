# Services and versions

The images `compose.yaml` pins, why those versions, and how to move them.
Pinned, not floating — reproducibility comes from the tags here plus
`Cargo.lock` for the Rust side, not from `latest`.

| Service | Image (pinned) | Role | Profile | State |
|---|---|---|---|---|
| app | built from `Dockerfile` (rust:__TEMPLATE_VERSION__-slim → debian:bookworm-slim) | the axum service | default | validated |
| redis | `redis:7.4-alpine` | read-through cache | default | upstream-available |
| rabbitmq | `rabbitmq:4.0-management-alpine` | `note.created` pub/sub | default | upstream-available |
| sonarqube | `sonarqube:lts-community` | code quality | quality | upstream-available |
| sonar-db | `postgres:16-alpine` | SonarQube's database | quality | upstream-available |

**State** — *validated*: generated and exercised by Delonix' test suite (the
project compiles and `cargo test` passes). *upstream-available*: the pinned tag
exists upstream and the field has been wired, but this environment has no
container runtime, so `compose up` and a real Sonar scan were **not** executed
here. Run them on a host with Docker/Podman before relying on them.

Validated on: 2026-10-22.

## Moving a version

- **Rust / app deps** — `Cargo.toml` pins each dependency with a caret floor;
  `cargo update` takes a patch/minor, `cargo update -p <crate> --precise` pins
  one. Commit `Cargo.lock` for reproducible images.
- **The toolchain** — the `-v` the project was generated with sets both the
  `rust-version` floor in `Cargo.toml` and the `FROM rust:<v>` build stage. Bump
  both in one deliberate commit.
- **Service images** — edit the tag in `compose.yaml`, bring the stack up on a
  real engine, run `make check` and `make scan`, then commit. Prefer pinning to
  a digest (`image: redis@sha256:…`) once you have validated one.

## Known prerequisites and limits

- **RabbitMQ** — the real broker glue is behind the `amqp` cargo feature (lapin
  is not compiled by default, so `cargo test` needs no broker). Build the image
  with `--features amqp` to publish and consume for real; the compose
  `rabbitmq` service is declared so it is one command away.
- **SonarQube** needs `vm.max_map_count=262144` on the host (Elasticsearch).
  `compose.yaml` does not change host sysctls; set it yourself (see README).
- **SonarQube community edition** analyses the main branch only.
- **`sonarqube:lts-community`** is a moving LTA tag; pin it to the exact version
  or a digest after your first validated scan.
- **Redis** runs without a password (internal Compose network, not published).
  Add `--requirepass` and a password in `REDIS_URL` before exposing it.
