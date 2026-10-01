#!/bin/sh
# Container entrypoint: optionally apply the migrations, then run the command
# (gunicorn by default).
#
# MIGRATE_ON_START=true is for ONE replica (the SQLite default of the
# manifest): the schema is created on first start and the service comes up
# ready. Leave it unset with several replicas — each would race to migrate —
# and run `manage.py migrate` as a release step instead. A failed migration
# stops the container (exit 1) before anything listens.
set -eu
cd /app
case "${MIGRATE_ON_START:-false}" in
  true|1|yes)
    /app/.venv/bin/python manage.py migrate --noinput
    ;;
esac
exec "$@"
