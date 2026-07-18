# __NAME__

A FastAPI service scaffolded by `delonix ... init --template python` — a complete,
best-practices starting point: layered code, health probes, tests, a multi-stage
non-root `Delonixfile`, and a Delonix manifest.

## Layout

```
Delonixfile              multi-stage build, non-root, HEALTHCHECK
delonix-manifest.yaml    declarative deploy (kind: Container)
pyproject.toml           deps + tooling
src/__MODULE__/          the app
  main.py                application factory
  config.py              typed settings (12-factor, env-driven)
  api/router.py          API v1 router
  api/health.py          /health/live + /health/ready
tests/                   pytest smoke tests
```

## Run it locally (no container)

```bash
pip install -e '.[dev]'
uvicorn __MODULE__.main:app --reload --app-dir src --port __PORT__
curl localhost:__PORT__/api/v1/health/live
pytest
```

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .     # build the image from the Delonixfile
delonix stack apply                 # bring it up (kind: Container)
delonix container ls
curl localhost:__PORT__/api/v1/health/live
```
