# __NAME__ (Odoo __TEMPLATE_VERSION__)

An Odoo stack scaffolded by `delonix init -t odoo -v __TEMPLATE_VERSION__` — the
official `odoo:__TEMPLATE_VERSION__` image plus a **PostgreSQL** container,
wired together over a Delonix bridge network with persistent volumes for
both, credentials in a `kind: Secret`, and OCA's own module-authoring tooling
already wired in. Odoo can't boot without a database, so this template ships
**manifests, not a single container**.

## Bring it up

**Development** (live reload — edit `addons/`/`config/odoo-dev.conf` on the
host, Odoo picks it up with no rebuild):

```bash
delonix build -t __NAME__:dev .                     # once, or after a Delonixfile change
delonix stack apply -f delonix-manifest.dev.yaml     # network → volumes → secret → postgres → odoo
```

**Production-shaped** (config and addons baked into the image):

```bash
delonix build -t __NAME__:dev .
delonix stack apply
```

The health probe lives at `/web/health`. What happens next depends on the
manifest:

- **Production-shaped** — `config/odoo.conf` sets `db_name = __NAME__`, so Odoo
  creates and initialises that database on its first start (about ten seconds
  on an idle disk, minutes on a busy one; `/web/health` answers once it is
  done). Open <http://localhost:__PORT__> and you get the login page. The web
  database manager is off (`list_db = False`) — that is why the database is
  named in the config rather than created in the UI.
- **Development** — the database manager is on: open
  <http://localhost:__PORT__> and create as many databases as you need.

### Change the factory password — before anything else

A database Odoo initialises itself has one user, `admin`, with the password
`admin`. Replace it before the port is reachable by anyone else:

```bash
printf 'env["res.users"].search([("login","=","admin")]).password = "%s"\nenv.cr.commit()\n' 'a-long-secret' \
  | delonix container exec -i __NAME__ /entrypoint.sh odoo shell -d __NAME__ --no-http
sh scripts/smoke.sh        # includes "the factory password admin/admin no longer works"
```

`/entrypoint.sh` is the official image's own wrapper: it adds the database
host and credentials from the container's environment, which a bare `odoo`
would not have.

> Heads-up: a lone `delonix container run __NAME__:dev` will **not** become
> healthy — Odoo needs the `db` container from the manifest. Always use
> `delonix stack apply`.

## What's in each manifest

| Resource | Name | Purpose |
|---|---|---|
| `Network` | `__NAME__-net` | bridge network; Odoo reaches Postgres by the alias `db` |
| `Volume`  | `__NAME__-dbdata` | PostgreSQL data (`/var/lib/postgresql/data`) |
| `Volume`  | `__NAME__-odoo` | Odoo filestore (`/var/lib/odoo`) |
| `Secret`  | `__NAME__-db-credentials` | one secret, both images' own env-var names (`POSTGRES_USER`/`POSTGRES_PASSWORD` for postgres, `USER`/`PASSWORD` for odoo) |
| `Container` | `__NAME__-db` | `postgres:16`, alias `db` |
| `Container` | `__NAME__` | `odoo:__TEMPLATE_VERSION__`, published on `__PORT__` |

`delonix-manifest.dev.yaml` additionally bind-mounts `addons/` and
`config/odoo-dev.conf` over what the image ships, instead of baking them in.

## Live reload in dev

`config/odoo-dev.conf` sets Odoo's own `dev_mode = all` — not a delonix
mechanism, the option Odoo itself has always shipped
(`odoo/tools/config.py`): it expands to `reload,qweb,xml`.

- **`reload`** — the server watches every addon's `.py` files and restarts
  itself on change.
- **`qweb`/`xml`** — templates and views are re-read from disk on every
  request, no restart at all.

So: edit a Python file under `addons/`, the server restarts itself in place;
edit a view/template, refresh the browser. No `delonix build`, no
`delonix stack apply` again — only a genuine image change (a new Python
dependency, a change to the Delonixfile itself) needs a rebuild.

## Custom modules, the OCA way

Drop each module (a directory with its own `__manifest__.py`) into `addons/`.
This template ships the same module-authoring tooling a real OCA repository
runs, wired and ready:

- **`.pre-commit-config.yaml`** — OCA's own quality gate
  ([OCA/pylint-odoo](https://github.com/OCA/pylint-odoo),
  [OCA/odoo-pre-commit-hooks](https://github.com/OCA/odoo-pre-commit-hooks),
  `ruff`/`ruff-format`, `prettier`, `eslint`, plus the generic whitespace/
  merge-conflict/XML checks). Install once, then it runs on every commit:
  ```bash
  pip install pre-commit
  pre-commit install
  pre-commit run --all-files   # first run: pulls its own tool environments
  ```
- **`.pylintrc`** — the full OCA check list (loaded by an IDE so nothing is
  silently skipped); **`.pylintrc-mandatory`** — the subset pre-commit
  actually blocks on. Both pin `valid-odoo-versions=__TEMPLATE_VERSION__`.
- **`.editorconfig`** — the same indent/charset/line-ending rules OCA module
  repos use, so an editor without the OCA extensions still formats correctly.
- **`requirements.txt`/`test-requirements.txt`** — where a dependency on
  another OCA repo's addon lands (`git+https://github.com/OCA/<repo>.git@__TEMPLATE_VERSION__#subdirectory=<addon>`),
  the current convention (replaced `oca_dependencies.txt`+git-aggregator).

The Delonixfile copies `addons/` to `/mnt/extra-addons` for the prod manifest;
the dev manifest bind-mounts it instead (see above).

## Commands

| Task | Command |
|---|---|
| Build | `delonix build -t __NAME__:dev .` |
| Run (prod-shaped / dev) | `delonix stack apply` / `delonix stack apply -f delonix-manifest.dev.yaml` |
| Smoke test | `sh scripts/smoke.sh` (dev: `SMOKE_PROFILE=dev sh scripts/smoke.sh`) |
| Logs | `delonix container logs -f __NAME__` |
| Odoo shell | `delonix container exec -it __NAME__ /entrypoint.sh odoo shell -d __NAME__ --no-http` |
| Install or update an addon | `delonix container exec __NAME__ /entrypoint.sh odoo -d __NAME__ -i <addon> --stop-after-init` (`-u` to update) |
| Run an addon's tests | `delonix container exec __NAME__ /entrypoint.sh odoo -d __NAME__-test -i <addon> --test-enable --test-tags /<addon> --stop-after-init --no-http` |
| Lint the addons | `pre-commit run --all-files` |
| PostgreSQL prompt | `delonix container exec -it __NAME__-db psql -U odoo postgres` |
| Stop the containers, keep the data | `delonix container stop __NAME__ __NAME__-db` |
| Remove EVERYTHING the stack owns — containers, network AND both data volumes | `delonix stack destroy` |

Addon tests run against a database of their own (`__NAME__-test` above) so a
test run never touches the data of the one being served. `--test-tags
/<addon>` is not optional: without it a fresh database also runs the test
suites of `base` and every dependency (about a thousand tests, some of which
fail outside Odoo's own CI), and the command exits non-zero for reasons that
have nothing to do with your addon. The last log line reads `0 failed, 0
error(s) of N tests`. That name does not
match `dbfilter`, which only affects HTTP requests — the command line is not
filtered.

## Credentials

`__NAME__-db-credentials` ships with the placeholder `odoo`/`odoo` in
cleartext `stringData` — `delonix stack apply` prints a WARNING about exactly
this, on purpose. Before a real deployment, replace it with a
`fromEnvFile: .secrets.env` (kept out of git — see `.dockerignore`) or run
`delonix secret create __NAME__-db-credentials --from-env-file - --force`
and drop `stringData` from the manifest.

## Production hardening already in `config/odoo.conf`

`db_name`/`dbfilter` (one database, named), `list_db = False`,
`proxy_mode = True`, `without_demo = True`, per-worker memory/CPU/request
limits, and logging to stdout — see the file itself for the reasoning behind
each. `workers = 0` ships as the safe default for a small deployment; size it
to `(2 * cpu_cores) + 1` once you outgrow it.

`proxy_mode = True` means Odoo trusts `X-Forwarded-*` headers. That is only
safe behind a reverse proxy that sets them — the manifest publishes
`__PORT__` on `127.0.0.1` for exactly that reason. Put a proxy in front
(`delonix init -t nginx` is one) before exposing it; Odoo itself sends no
security headers and does not terminate TLS.

## What this template does not do

- **It does not migrate a filestore volume written as root.** Both manifests
  run Odoo as the image's own user (`user: odoo`). A NEW `__NAME__-odoo` volume
  is handed to that user at its first mount; one that an earlier run filled as
  root keeps its owner, and Odoo then cannot write to it. Remove the volume, or
  fix its ownership from a one-off container running as root.
- **It does not let Odoo write into `addons/` in development.** A bind mount
  keeps the ownership your host gave it; Odoo reads your addons and does not
  write there.
- **No OpenTelemetry.** Odoo has no built-in exporter; what you get is its
  own log on stdout (`delonix container logs`), one line per request with
  timing and query counts.
- **No backups.** The data is in the `__NAME__-dbdata` and `__NAME__-odoo`
  volumes, and `delonix stack destroy` deletes both. Archive first:
  `delonix backup create container __NAME__-db` (and `__NAME__`).
- **The first production start can outlast `--up`.** `delonix init --up`
  waits a bounded time for `/web/health`; on a very busy disk the database
  initialisation can take longer, `--up` then reports a failure, and Odoo
  finishes on its own — check with `sh scripts/smoke.sh`.

## On the internet without a public IP

`delonix-tunnel.yaml` opens an OUTBOUND tunnel: __NAME__ gets an `https`
address on the internet with no public IP, nothing opened on the router, and
no certificate to manage — the provider serves HTTPS with its own. It runs
only when applied; `stack apply` on `delonix-manifest.yaml` does not touch it.
Anyone on the internet can then reach the service.

Change the factory password first (see "Change the factory password —
before anything else"): the tunnel puts the login page on the internet.
`proxy_mode = True` in `config/odoo.conf` already makes Odoo build its links
from the host and scheme the tunnel reports.

### Just a tunnel (no account, no domain)

1. Install `cloudflared` ([releases](https://github.com/cloudflare/cloudflared/releases),
   or your distribution's package) somewhere on `PATH`.
2. `delonix stack apply -f delonix-tunnel.yaml`
3. `delonix get gateways` — the address is in `PUBLIC URL`.

The address is random (`https://<words>.trycloudflare.com`) and changes every
time the tunnel opens; Cloudflare offers these quick tunnels for testing,
without an uptime guarantee. Close it with
`delonix delete gateways __NAME__-tunnel`.

### Your own domain (a stable address)

The domain's DNS has to be on Cloudflare (a free account is enough: point the
domain's name servers, at your registrar, to the two Cloudflare gives you).

1. Cloudflare dashboard → Zero Trust → Networks → Tunnels → **Create a
   tunnel** → Cloudflared. Name it and copy the token from the install
   command it shows (`--token eyJ…`).
2. In the tunnel, add a **public hostname**: `app.example.org` → service
   `http://localhost:__PORT__`. Cloudflare creates the DNS record.
3. Keep the token in a Delonix secret, typed in so it lands in no file and
   no shell history: run `delonix secret create __NAME__-tunnel --from-env-file -`,
   type `token=<the token>`, then Enter and Ctrl-D.
4. In `delonix-tunnel.yaml`, uncomment `tokenSecretRef` and `hostname`, then
   `delonix stack apply -f delonix-tunnel.yaml`.

Visitors get Cloudflare's certificate for `app.example.org`. To change the
route later, change it in the dashboard: with a token the tunnel takes its
routes from there, not from this file.

### Other providers

`provider:` also takes:

- `ngrok`: the ngrok agent on `PATH` and an account
  (`ngrok config add-authtoken <token>`). A free account has one fixed name
  (`<words>.ngrok-free.dev`): a stable address without a domain of your own.
  One ngrok tunnel at a time per machine.
- `pinggy`: nothing to install (plain `ssh`), a URL valid for 60 minutes, and a warning page shown to browsers before the site.

## Tests and CI

```bash
pre-commit run --all-files    # pylint (OCA rules), ruff, prettier, XML checks
sh scripts/smoke.sh           # against a running instance
```

`pre-commit` is what `.github/workflows/ci.yml` and `.gitlab-ci.yml` both run.
It covers the addons you write under `addons/`; the Delonix/CI glue this
template generates is excluded in `.pre-commit-config.yaml`, because those
files are not addon code.

Addon TESTS need a live database and are deliberately not in that gate — run
them with `--test-tags /<addon>` against a running instance (above).

`sonar-project.properties` is ready for a `sonar-scanner` run; `CONTRIBUTING.md`
has the branching model, Conventional Commits and SemVer.

## Architecture: where to take this

**A rule before the menu: an architecture no test enforces is not an
architecture, it is an intention.** This project ships one — `pre-commit run --all-files` (OCA's own rules), run in CI — and the
paths below each say what changes *in that test*. Move the folders without
moving the test and the dependency direction is back to where it was within a
few sprints.

### What this project is today

An Odoo deployment, not an application you wrote: the ERP is the image, the
database and the network are in `delonix-manifest.yaml`, and `addons/` is where
YOUR code goes. The architectural decisions here are about what belongs in an
addon, what belongs outside Odoo, and how you keep upgrades survivable.

### Path 1 — one addon per bounded area

An addon is already a module boundary with a manifest and declared
dependencies. Use it: one addon per area, depending on the standard modules and
on each other explicitly. *Patterns:* Module per area, explicit dependency in
`__manifest__.py`. **Do not create one `custom` addon that grows forever** — it
is the single most common Odoo mistake and it makes every upgrade a rewrite.

### Path 2 — extend, do not fork

Inherit (`_inherit`, view inheritance) instead of copying standard code.
*Patterns:* Open/closed by inheritance, delegation. *The sign you chose wrong:*
an upgrade of Odoo means re-applying your diffs by hand.

### Path 3 — integrations outside the ERP

For anything that is not an ERP concern — a public API, a high-traffic
front-end, a heavy integration — put a separate service beside Odoo on the same
Delonix network and talk to it over its API, rather than growing an addon that
serves HTTP to the world. *Patterns:* Anti-corruption layer, Outbox for events
leaving Odoo. **Keep the ERP's web surface private**; the templates `nginx` and
`haproxy` are the edge for anything public.
### Choosing

| If your problem is… | Go to | The sign you chose wrong |
|---|---|---|
| "a CRUD endpoint takes all afternoon" | nothing — the layout is not the problem | you are adding folders and the endpoint still takes all afternoon |
| "I cannot test a rule without a database" | you already have the answer: keep rules off the adapters | a unit test that needs Docker |
| "two features keep breaking each other" | modular: one folder per bounded area, public surface only | a fix in one area ships a regression in another |
| "the read path and the write path pull in opposite directions" | split the read model (CQRS), with a number to back it | two models to keep in step and nobody remembers why |
| "this side effect does not belong in the request" | events + outbox | the user waits for something they did not ask for |
| "two teams block each other on every deploy" | separate services — AFTER the modules are clean | a deploy of one service breaks another |

### What not to do yet

- **Do not add a layer because it reads well.** Every indirection is paid on
  every change; add one when a concrete change is hard, not before.
- **Do not go distributed to get modularity.** A module boundary that a test
  enforces gives you the decoupling; a network hop gives you retries,
  timeouts, partial failures and a tracing bill.
- **Do not let the persistence shape the rules.** The moment a use case
  mentions a column name, the next storage decision is a rewrite.
- **Do not change the architecture and the architecture test in different
  commits.** They are one change.

See also the local `ARCHITECTURE.md` for this project's current rules.

## Troubleshooting

- **"Odoo database manager has been disabled"** — you are on the production
  config and `db_name` does not name an existing or creatable database; check
  `config/odoo.conf` and the container log.
- **`invalid addons directory '/mnt/extra-addons', skipped`** in the log —
  `addons/` has no module yet. It goes away with the first directory that has
  an `__init__.py` and a `__manifest__.py`.
- **The Odoo container exits right after a host reboot** — PostgreSQL was
  still recovering and the image's entrypoint gave up waiting; `restart:
  always` in the manifest starts it again.
