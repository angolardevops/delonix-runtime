# __NAME__ (Django __TEMPLATE_VERSION__)

A Django project scaffolded by `delonix init -t django -v __TEMPLATE_VERSION__` —
**uv**-managed and pinned to `django==__TEMPLATE_VERSION__.*` (a patch/point
release lands with a plain `uv sync`; a major upgrade needs a new `-v`),
12-factor settings, health probes, gunicorn, and a Delonix manifest. Minimal
(no DB) so it runs out of the box; add `DATABASES` + `django.contrib.*` when
you add models.

## Run it locally (uv)

```bash
uv sync                                    # create .venv + install (incl. dev)
uv run python manage.py runserver __PORT__
curl localhost:__PORT__/api/v1/health/live
uv add <package>                           # add a dependency (updates pyproject.toml)
```

Don't have uv? Install it: `curl -LsSf https://astral.sh/uv/install.sh | sh`.

## Choosing a Django version

Regenerate with `-v` to pick another line — a bare major (`-v 5`) or
`major.minor` (`-v 5.2`):

```bash
delonix init -t django -v 5.2 --force .
```

`--force` is required: `init` never overwrites without it. Omit `-v`
entirely and it falls back to this template's own default
(`__TEMPLATE_VERSION__`, from `template.meta`).

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```

## Contributing

CI (`.github/workflows/ci.yml` for GitHub, `.gitlab-ci.yml` for GitLab) runs
`ruff check`/`manage.py check` via `uv` on every push and PR. `sonar-project.
properties` is ready for a SonarQube/SonarCloud scan. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the branching model (git-flow), commit
message format (Conventional Commits) and versioning (SemVer).
