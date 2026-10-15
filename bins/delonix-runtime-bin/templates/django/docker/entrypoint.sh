#!/bin/sh
# Container entrypoint: optionally apply the migrations, create admin user,
# then run the command (gunicorn by default).
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

    # Create default admin user with random password
    ADMIN_USER="${ADMIN_USER:-admin}"
    ADMIN_EMAIL="${ADMIN_EMAIL:-admin@example.com}"
    ADMIN_PASSWORD="${ADMIN_PASSWORD:-$(openssl rand -base64 16)}"

    # Create the admin user if it doesn't exist
    /app/.venv/bin/python manage.py shell <<EOF
from django.contrib.auth.models import User
import os

admin_user = "$ADMIN_USER"
admin_email = "$ADMIN_EMAIL"
admin_password = "$ADMIN_PASSWORD"

if not User.objects.filter(username=admin_user).exists():
    User.objects.create_superuser(admin_user, admin_email, admin_password)
    print("\n" + "="*60)
    print("Django Admin User Created")
    print("="*60)
    print(f"URL:      http://localhost:__PORT__/admin/")
    print(f"Username: {admin_user}")
    print(f"Password: {admin_password}")
    print("="*60 + "\n")
else:
    print(f"Admin user '{admin_user}' already exists")
EOF
    ;;
esac
exec "$@"
