# __NAME__ (Odoo 19)

An Odoo stack scaffolded by `delonix init --template odoo` — the official
`odoo:19` image plus a **PostgreSQL** container, wired together over a Delonix
bridge network with persistent volumes for both. Odoo can't boot without a
database, so this template ships a **manifest, not a single container**.

## Bring it up

```bash
delonix build -t __NAME__:dev .     # builds the Odoo image (+ your addons/)
delonix stack apply                 # network → volumes → postgres → odoo
```

Then open <http://localhost:__PORT__> and create your first database in the web
UI (master password is set on first run). The health probe lives at
`/web/health`.

> Heads-up: a lone `delonix container run __NAME__:dev` will **not** become
> healthy — Odoo needs the `db` container from the manifest. Always use
> `delonix stack apply`.

## What's in the manifest

| Resource | Name | Purpose |
|---|---|---|
| `Network` | `__NAME__-net` | bridge network; Odoo reaches Postgres by the alias `db` |
| `Volume`  | `__NAME__-dbdata` | PostgreSQL data (`/var/lib/postgresql/data`) |
| `Volume`  | `__NAME__-odoo` | Odoo filestore (`/var/lib/odoo`) |
| `Container` | `__NAME__-db` | `postgres:16`, alias `db` |
| `Container` | `__NAME__` | `odoo:19`, published on `__PORT__` |

## Custom modules

Drop each module (a directory with its own `__manifest__.py`) into `addons/`.
The Delonixfile copies them to `/mnt/extra-addons`; rebuild with
`delonix build -t __NAME__:dev .` and `delonix stack apply` again.

> The default `odoo`/`odoo` Postgres credentials are for local development.
> In production, set them via `delonix secret` and reference the secrets from
> the manifest's `env` instead of hard-coding them.
