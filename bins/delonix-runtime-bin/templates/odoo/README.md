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

Then open <http://localhost:__PORT__> and create your first database in the web
UI. The health probe lives at `/web/health`.

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

## Credentials

`__NAME__-db-credentials` ships with the placeholder `odoo`/`odoo` in
cleartext `stringData` — `delonix stack apply` prints a WARNING about exactly
this, on purpose. Before a real deployment, replace it with a
`fromEnvFile: .secrets.env` (kept out of git — see `.dockerignore`) or run
`delonix secret create __NAME__-db-credentials --from-env-file - --force`
and drop `stringData` from the manifest.

## Production hardening already in `config/odoo.conf`

`list_db = False`, `proxy_mode = True`, `without_demo = all`, per-worker
memory/CPU/request limits, and logging to stdout — see the file itself for
the reasoning behind each. `workers = 0` ships as the safe default for a
small deployment; size it to `(2 * cpu_cores) + 1` once you outgrow it (and
switch a reverse proxy in front — `proxy_mode` already expects one).
