# The image compose.yaml and `docker build` use. It is the same two-stage uv
# build as the Delonixfile (which `delonix build` uses and may extend with
# Delonix-only directives): keep the two in step when you change either.
#
#   docker build -t __NAME__:dev .          # or: docker compose build
#
# Stage 1 resolves the virtual environment (deps + this project, non-editable,
# byte-compiled); only that environment reaches the runtime image — no uv, no
# build cache, no source tree, no tests. .dockerignore keeps .env and .venv out
# of the context.
FROM python:3.13-slim AS build
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy UV_PYTHON_DOWNLOADS=never UV_PROJECT_ENVIRONMENT=/app/.venv
RUN pip install --no-cache-dir --disable-pip-version-check uv==0.12.21
WORKDIR /app
COPY . /app
# From the lock when the context has one (reproducible); a fresh project that
# has not run `uv lock` yet resolves here instead, and says so.
RUN --mount=type=cache,target=/root/.cache/uv if [ -f uv.lock ]; then uv sync --locked --no-dev --no-editable; else echo "uv.lock not found: resolving now; run 'uv lock' and commit it for reproducible images" && uv sync --no-dev --no-editable; fi

FROM python:3.13-slim
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 PATH=/app/.venv/bin:$PATH
WORKDIR /app
COPY --from=build /app/.venv /app/.venv
# An unprivileged user for the service (uid 10001). It owns nothing in the image
# and writes only to the manifest's tmpfs.
RUN groupadd --system --gid 10001 app && useradd --system --uid 10001 --gid app --no-create-home --home-dir /nonexistent app
EXPOSE __PORT__
# The runtime probes liveness; readiness is for the load balancer.
HEALTHCHECK CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:__PORT__/api/v1/health/live', timeout=3)" || exit 1
STOPSIGNAL SIGTERM
USER app
# The project's own entry point (server.py): readiness, drain and exit code.
CMD ["python", "-m", "__MODULE__"]
