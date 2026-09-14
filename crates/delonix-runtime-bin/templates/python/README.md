# __NAME__ (FastAPI __TEMPLATE_VERSION__)

A FastAPI service scaffolded by `delonix init -t python -v __TEMPLATE_VERSION__`
— pinned to `fastapi==__TEMPLATE_VERSION__.*`, a complete, best-practices
starting point: **uv**-managed, layered code, health probes, tests, a
single-stage non-root-friendly `Delonixfile`, and a Delonix manifest.

## Layout

```
pyproject.toml           project + deps (uv), dev group (PEP 735), ruff config
.python-version          the Python uv provisions
Delonixfile              uv-native build, HEALTHCHECK
delonix-manifest.yaml    declarative deploy (kind: Container)
src/__MODULE__/          the app
  main.py                application factory
  config.py              typed settings (12-factor, env-driven)
  api/router.py          API v1 router
  api/health.py          /health/live + /health/ready
tests/                   pytest smoke tests
```

## Run it locally (uv)

```bash
uv sync                                    # create .venv + install (incl. dev)
uv run uvicorn __MODULE__.main:app --reload --app-dir src --port __PORT__
curl localhost:__PORT__/api/v1/health/live
uv run pytest
uv add <package>                           # add a dependency
```

Don't have uv? Install it: `curl -LsSf https://astral.sh/uv/install.sh | sh`.

## Choosing a FastAPI version

Regenerate with `-v <major.minor>` to pin another line, e.g.
`delonix init -t python -v 0.116 --force .` (`--force`: `init` never
overwrites without it). Omit `-v` and it falls back to this template's own
default (`__TEMPLATE_VERSION__`, from `template.meta`).

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .     # build the image from the Delonixfile
delonix stack apply                 # bring it up (kind: Container)
delonix container ls
curl localhost:__PORT__/api/v1/health/live
```

## Contributing

CI (`.github/workflows/ci.yml` for GitHub, `.gitlab-ci.yml` for GitLab) runs
`ruff check`/`pytest` via `uv` on every push and PR. `sonar-project.properties`
is ready for a SonarQube/SonarCloud scan. See [CONTRIBUTING.md](CONTRIBUTING.md)
for the branching model (git-flow), commit message format (Conventional
Commits) and versioning (SemVer).
