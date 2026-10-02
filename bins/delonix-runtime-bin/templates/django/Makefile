# Every command the README, CI and AGENTS.md mention is a target here.
# CI sets UV_LOCKED=1, so every `uv run` there refuses a missing or stale uv.lock.
PORT ?= __PORT__

.PHONY: install lock fmt fmt-check lint test migrations-check deploy-check vuln migrate dev run smoke check

install:           ## create .venv and install (writes uv.lock on the first run — commit it)
	uv sync
lock:              ## re-resolve dependencies into uv.lock
	uv lock
fmt:               ## rewrite files with ruff format, apply safe lint fixes
	uv run ruff format .
	uv run ruff check --fix .
fmt-check:         ## fail if a file is not formatted
	uv run ruff format --check .
lint:
	uv run ruff check .
test:              ## unit, integration, contract (OpenAPI drift), architecture and trace tests
	uv run pytest
migrations-check:  ## fail if a model changed without its migration
	APP_ENV=test uv run python manage.py makemigrations --check --dry-run
deploy-check:      ## Django's deployment checklist against production settings; warnings fail
	APP_ENV=production ALLOWED_HOSTS=example.com DATABASE_URL=sqlite:///deploy-check.sqlite3 \
	TRUSTED_PROXY=127.0.0.1 SECRET_KEY=$$(python3 -c 'import secrets; print(secrets.token_urlsafe(50))') \
	uv run python manage.py check --deploy --fail-level WARNING
vuln:              ## known vulnerabilities in the locked dependencies (needs network)
	uv export --locked --no-dev --no-emit-project --format requirements-txt -o .audit-requirements.txt -q
	uvx pip-audit@2.10.0 --require-hashes --disable-pip -r .audit-requirements.txt; rc=$$?; rm -f .audit-requirements.txt; exit $$rc
migrate:           ## apply database migrations (an explicit step, never done by the server)
	uv run python manage.py migrate
dev:               ## development server with autoreload (no graceful shutdown)
	uv run python manage.py runserver $(PORT)
run:               ## the production server: gunicorn, configured by gunicorn.conf.py
	PORT=$(PORT) uv run gunicorn config.wsgi:application
smoke:             ## against a running instance: make smoke URL=http://localhost:__PORT__
	sh ./scripts/smoke.sh $(or $(URL),http://localhost:$(PORT))
check: fmt-check lint migrations-check deploy-check test  ## what CI runs
