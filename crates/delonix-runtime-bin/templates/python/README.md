# __NAME__

A FastAPI service scaffolded by `delonix ... init --template python` — a complete,
best-practices starting point: **uv**-managed, layered code, health probes, tests,
a single-stage non-root-friendly `Delonixfile`, and a Delonix manifest.

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

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .     # build the image from the Delonixfile
delonix stack apply                 # bring it up (kind: Container)
delonix container ls
curl localhost:__PORT__/api/v1/health/live
```
