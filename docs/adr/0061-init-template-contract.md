# ADR-0061: What a `delonix init` template promises, and how the generator keeps it

- **Status:** Proposed (2026-09-30) — the application-template contract, the lock policy
  and the generator checks are implemented on `init/templates-master`; the owner decides
- **Date:** 2026-09-30
- **Deciders:** Walter Angolar
- **Relates to:** `bins/delonix-runtime-bin/src/cmd/{init,scaffold,build}.rs`, the templates
  under `bins/delonix-runtime-bin/templates/`, PR #625 (the `fastapi` rename and Odoo 20)

## Context

`delonix init` embeds eleven templates. Measured against the binary of `9eb2a14d`
(2026-09-30), generating each one into an empty directory and running the commands its own
README and CI give:

| finding | evidence |
|---|---|
| the documented CI fails on a fresh project | django `uv run ruff check .` rc=1; node `pnpm lint` rc=1 (`eslint: not found`); django `pytest` rc=5 (no tests) |
| `-v` is copied into manifests unchecked | `-v '5.2.*", "evil==1'` added a dependency `evil==1.*` to `pyproject.toml`; rc=0 |
| the project name is copied unchecked | a directory `My App"x` produced an unparsable `package.json`; `Weird Go` produced `module Weird Go`; rc=0 |
| incompatible version combinations accepted | `-t nextjs -v 14.2.0` kept `react ^19` (Next 14 peers on React 18) |
| defaults past end of life | Node 20 (EOL 2026-04-30), Go 1.23 (EOL 2025-08-12), Django 5.1 (EOL 2025-12-03), nginx 1.27, NestJS 10 and Next 15 two/one majors behind |
| adoption writes a CI that cannot run | an npm project with `package-lock.json` and its own workflow got a pnpm `ci.yml`, a `.gitlab-ci.yml` and a pnpm Delonixfile |
| image claims the build could not keep | `Delonixfile` comments said `delonix build` is single-stage (it is not); `.dockerignore` files were never read by `delonix build`, so `COPY . .` took `.env` into a layer (fixed in `c910fe3d`) |
| the example service is two probes | no business path, readiness always 200, no shutdown, no telemetry, no contract, no webhooks |

Lifecycle sources: endoflife.date API, read 2026-09-30 (`nodejs`, `go`, `python`, `django`,
`laravel`, `php`, `nextjs`, `nginx`, `haproxy`); npm registry for NestJS/Fastify.

## Decision

### D1 — An application template generates a small, complete service

Applies to `go`, `node`, `nestjs`, `nextjs`, `fastapi`, `django`, `laravel`. Not to `odoo`
(an ERP deployment: addons and configuration in the Odoo/OCA shape) nor to `nginx`,
`httpd`, `haproxy` (infrastructure: syntax check, smoke, health, logs, headers).

Every application template has, in the framework's own idiom:

1. **One capability end to end** — *notes*: `POST /api/v1/notes` (title 1–200 chars after
   trim, body ≤ 10 000), `GET /api/v1/notes?offset&limit` (limit 1–100, default 20, newest
   first, `{items,total,offset,limit}`), `GET /api/v1/notes/{id}`. Transport → validation →
   use case → port → adapter (in memory unless the framework's persistence is the idiom).
2. **One error shape**: `{"error":{"code","message","fields"?,"request_id"}}` with codes
   `validation_failed` (422), `malformed_json` (400), `body_too_large` (413), `not_found`
   (404), `internal` (500, no stack).
3. **Configuration** typed and validated at startup, `.env.example` without real secrets,
   `APP_ENV` = `development|test|production`, production refusing dev shortcuts.
4. **Lifecycle**: `GET /api/v1/health/live` (process) and `/api/v1/health/ready`
   (503 `starting` → 200 `ready` → 503 `draining` after SIGTERM/SIGINT), shutdown bounded by
   `SHUTDOWN_TIMEOUT`, in-flight requests finished, clients and exporters flushed.
5. **Webhooks in both directions**, in the Standard Webhooks format: inbound
   `POST /api/v1/webhooks/inbound` (only when `WEBHOOK_INBOUND_SECRET` is set; raw-body HMAC,
   5-minute window, constant-time compare, de-duplication by `webhook-id`, body limit), and an
   outbound signed `note.created` to `WEBHOOK_TARGET_URL` with bounded retries, full-jitter
   backoff and the note id as idempotency key. Delivery semantics written down, never
   exactly-once. Where the framework has a native queue (Laravel), it is used.
6. **Observability**: JSON logs to stdout with service/version/environment/request_id and
   trace_id/span_id, keys naming secrets redacted; OpenTelemetry traces (and metrics where
   the language SDK is stable) exported over OTLP/HTTP only when an endpoint is configured,
   health probes excluded, a collector being down never failing a request.
7. **Contract**: `api/openapi.yaml` (or the framework's generated OpenAPI with a checked-in
   copy) and a test that fails when routes and contract drift.
8. **Gates as tests**: dependency direction, contract drift, and one trace across request,
   use case, outbound call and log lines — each shown to fail on an injected defect.
9. **Harness**: `README.md` (the twelve sections of the master prompt), `AGENTS.md`,
   `ARCHITECTURE.md` with an extension walk-through, `docs/adr/`, `docs/runbooks/`, a
   `Makefile` (or the ecosystem's task runner) holding every command the docs cite, CI with
   a read-only token and actions pinned by SHA, Dependabot.

### D2 — Locks: shipped when they are a function of the manifest, generated otherwise

- **Go**: `go.sum` is shipped. `go.mod` pins exact versions, so the hashes are fixed, and
  `-v` changes only the toolchain directive.
- **uv, pnpm, Composer**: no lock is shipped. A lock records resolved versions of ranges, and
  `-v` changes the ranges; a shipped lock would be stale for every `-v`. The first
  `uv lock` / `pnpm install` / `composer install` writes it; the README says to commit it;
  CI installs with `--locked` / `--frozen-lockfile` / `composer install` from the lock and
  fails, with the reason, when it is missing. The Delonixfile installs from the lock when
  the context has one and resolves otherwise, so `init --up` still works on a fresh project.
- `init` never runs a package manager: no network, no shell, at generation time.

### D3 — The generator checks what it substitutes

- The project name must be a DNS-label-style slug (`[a-z0-9]` and inner `-`, ≤ 63), because
  it becomes a container name, an image tag, an npm/Composer/Go module name and a YAML
  scalar. An explicit `--name` that is anything else is refused before a file is written,
  with a suggested slug. A name DERIVED from the directory (`My App`, `Shop_API`) is turned
  into the nearest slug and said (`using 'my-app'`): a directory is named for people, and
  before this ADR such a directory was accepted, so refusing it would break a command that
  used to work.
- `-v` must be a plain version (`N`, `N.N`, `N.N.N`) and inside the template's declared
  support range (`versions=` in `template.meta`, majors or major.minor); outside, refused
  with the range. Combinations the framework does not support (Next 14 with React 19) are
  expressed by the range, not left to the user to discover.
- Every destination is checked for symlinks before the first file is written, so a refusal
  leaves the directory as it was; an executable template file keeps its executable bit.
- `template.meta` carries what the generator needs to know about a template: `port=`,
  `health=`, `version=`, `versions=`, `lock=` (the package manager's lock file, read by
  adoption) and `wait=` (seconds `--up` waits for health; default 120).
  Optional keys are read by name (`TEMPLATE_KV`): `tls=<port>` (the template serves HTTPS;
  `init` generates the certificate into `./tls` and `__TLS_PORT__` is substituted), `open=`
  (the path `--up` prints as the address to open), and `login=`/`password=` (factory
  credentials `--up` prints with the warning to change them).
- One test renders every embedded template for every version it declares and parses the
  JSON and YAML it wrote, so a template added later is covered without a new test.

### D4 — Detection reads manifests, and says "unknown" when it does not know

`package.json` dependencies, `pyproject.toml`/`requirements.txt` requirements and
`composer.json` `require` decide the framework. A PHP project without `laravel/framework`, a
Python project without FastAPI or Django, or a Node project on another framework is
**unknown**: the generic scaffold, with the reason printed. An explicit `-t` always wins over
detection, including over a compose file or a VMfile (with a warning naming the file).

### D5 — Adoption writes only what the project can run

In a non-empty directory only the Delonix glue is written. CI files are written only when
the project has no CI of its own and uses the template's package manager (lock file
present); otherwise they are skipped and the reason printed. Existing files are never
overwritten without `--force`.

### D6 — gRPC is not generated

A gRPC server is a second transport over the same use cases; it needs a `.proto`, a
reproducible code generator per language, lint/breaking checks and TLS decisions. No
consumer of these templates has asked for it. Deferred until one does; the ARCHITECTURE.md
of each backend template names where a second transport would plug in.

### D7 — Supported versions (defaults, 2026-09-30)

| template | default `-v` | accepted | runtime |
|---|---|---|---|
| go | 1.26 | 1.26, 1.27 | toolchain = `-v` |
| node (Fastify) | 5 | 5 | Node 24 LTS |
| nestjs | 12 | 11, 12 | Node 24 LTS |
| nextjs | 16 | 15, 16 | Node 24 LTS, React 19 |
| fastapi | 0.142 | 0.141, 0.142 | Python 3.13 |
| django | 5.2 (LTS) | 5.2, 6.0, 6.1 | Python 3.13 |
| laravel | 13 | 12, 13 | PHP 8.4 |

### D8 — What the image build had to learn

Three things the templates rely on were measured to be false in `delonix build` and fixed in
the engine rather than worked around in every Delonixfile:

- `.dockerignore` was never read (`COPY . .` took `.env` and `.git` into a layer);
- `COPY a b dst/` copied `a` and dropped `b` silently;
- a symlink inside a copied tree was followed, so `COPY --from=build /app/node_modules`
  failed on pnpm's directory links.

A second pass fixed the first fix: with any `!` rule, every excluded directory was walked
and left as an empty skeleton (a `.venv` skeleton made `uv sync` refuse to create the
environment). An excluded directory is entered only when an exception can match inside it.

A third defect was found by the images themselves: the last stage's `ENV` was packaged
twice, the second time unexpanded, so `ENV PATH=/app/.venv/bin:$PATH` produced an image whose
PATH was that literal string (`id: not found` inside the container; the service ran because
its command lives in the one directory left). The packaged environment is now the stage's
own, expanded once.

### D9 — The images run as an unprivileged user

Each Delonixfile ends in `USER` and each manifest names the same user with `user:` — the
engine applies an image's USER only when asked (ADR-0062 P0), and D1 of that ADR makes the
manifest line redundant once it lands. The user owns nothing in the image except the mount
point of a volume the service writes to, which an empty named volume takes over on its first
mount (ADR-0062 D3).

Measured on both engines, with the `go` and `django` images: built from this branch alone
(before ADR-0062 P1) and built with P1 merged in, the process runs as the unprivileged user,
`/etc/passwd` and the application files stay `0:0`, appending to `/etc/passwd` is refused,
and django's `/data` volume is owned by the user and writable. The templates do not wait for
P1. What P1 changes for them is how the volume gets its owner (the image's owner of the mount
point, instead of a walk).

A host with no subordinate uid range cannot run a second user; there the `user:` line is
removed and the container runs as uid 0 mapped to the invoking user. Not measured on such a
host.

### D10 — A fresh deployment comes up ready

`delonix init -t <template> --up` must end with a service that answers ready. Two templates
did not: `django` needed a manual `manage.py migrate`, and `laravel` needed a secret (still
true — production refuses to start without `APP_KEY`, and a key is never generated into a
file the project commits). `django` now migrates at start when `MIGRATE_ON_START=true`, which
its manifest sets because the default is one replica on SQLite; with several replicas the
variable is removed and the migration is a release step.

### D11 — The generated CI is executed, not only parsed

`scripts/init-ci.sh` generates each application template and runs its CI in a clean rootless
container: the image the project's `.gitlab-ci.yml` names, the lock created by the command
the README gives, then the GitLab job as written and every `run:` step of the GitHub
workflow. The `uses:` actions of the GitHub workflow (checkout, toolchain setup) are the one
part not executed; the GitLab image and `before_script` stand in for them.

## Alternatives considered

- **Ship locks for every ecosystem.** Rejected for uv/pnpm/Composer by D2: stale for any
  `-v`, and a lock that disagrees with the manifest fails `--frozen` installs on the first CI
  run.
- **Profiles/flags per capability (`--with-grpc`, `--with-db`).** Multiplies every template
  by every flag; deferred until a second profile is asked for.
- **Resolve dependencies during `init`.** Makes `init` depend on the network and on
  toolchains being installed; rejected.

## Consequences

- A fresh project's CI fails until the lock is committed, with a message saying so. That is
  the intended first step, written in each README.
- Older framework majors fall outside `-v`; users who need them edit the manifest by hand.
- Templates are larger; each carries its own tests, so a regression in a template is caught
  by running that template's own `make check`.
