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
