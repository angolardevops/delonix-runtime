# Every command the README, CI and AGENTS.md mention is a target here.
# `uv run --locked` never rewrites uv.lock: a stale or missing lock is an error.
RUN := uv run --locked
URL ?= http://localhost:__PORT__

.PHONY: lock install fmt fmt-check lint typecheck test cover audit build openapi run dev smoke check

lock:        ## resolve the dependencies and write uv.lock (commit it)
	uv lock
install: uv.lock  ## create .venv exactly as locked
	uv sync --locked
uv.lock:
	@echo "uv.lock is missing. Run 'make lock' (or 'uv lock') and commit uv.lock:" >&2
	@echo "installs are reproducible only from the lock, and CI refuses to resolve." >&2
	@exit 1
fmt:         ## rewrite files (format, then the lint fixes that are safe)
	$(RUN) ruff format .
	$(RUN) ruff check --fix .
fmt-check:   ## fail if a file is not formatted
	$(RUN) ruff format --check .
lint:
	$(RUN) ruff check .
typecheck:   ## mypy --strict on src/, tests type-checked too
	$(RUN) mypy
test:        ## unit, HTTP, webhook, one-trace, contract-drift and architecture tests
	$(RUN) pytest
cover:
	$(RUN) pytest --cov --cov-report=term --cov-report=xml
audit:       ## known vulnerabilities in the locked runtime dependencies (network)
	uv export --locked --no-dev --no-emit-project --format requirements-txt -o .audit-requirements.txt -q
	uvx pip-audit@2.10.1 --disable-pip --require-hashes -r .audit-requirements.txt
build:       ## sdist and wheel in dist/
	uv build
openapi:     ## regenerate the checked-in contract from the routes
	$(RUN) python -m __MODULE__.contract > api/openapi.json.tmp
	mv api/openapi.json.tmp api/openapi.json
run:         ## the production entry point
	$(RUN) python -m __MODULE__
dev:         ## auto-reload on change
	$(RUN) uvicorn __MODULE__.server:dev_app --factory --reload --port __PORT__
smoke:       ## against a running instance: make smoke URL=http://localhost:__PORT__
	sh ./scripts/smoke.sh $(URL)
check: install fmt-check lint typecheck test build  ## what CI runs
