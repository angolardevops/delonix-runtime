#!/bin/sh
# Container entrypoint. `serve` (default): validate the configuration, cache
# it, migrate, then run FrankenPHP. `worker`: validate, wait for the schema,
# then run the queue worker that delivers outbound webhooks.
set -eu
cd /app
mode="${1:-serve}"

# Refuse to start on a bad configuration (every problem listed, exit 1)
# BEFORE anything listens. APP_KEY is required here, in production.
php artisan app:check-config
# Cached from the RUNTIME environment (secrets included), into /tmp
# (APP_CONFIG_CACHE), never at build time. The directory is created here, not
# in the image: the manifest mounts a tmpfs on /tmp, which hides anything the
# build put there (measured: the container exited 1 with no message).
mkdir -p "$(dirname "${APP_CONFIG_CACHE:-/tmp/laravel/config.php}")"
php artisan config:cache --no-ansi >/dev/null

case "$mode" in
  serve)
    db="${DB_DATABASE:-}"
    if [ "${DB_CONNECTION:-sqlite}" = sqlite ] && [ -n "$db" ] && [ ! -e "$db" ]; then
      touch "$db"
    fi
    php artisan migrate --force --no-interaction
    exec frankenphp run --config /app/Caddyfile
    ;;
  worker)
    # The web container migrates; wait (bounded) until it has.
    i=0
    until php artisan migrate:status --pending >/dev/null 2>&1; do
      i=$((i + 1))
      if [ "$i" -ge 60 ]; then echo "worker: schema not migrated after 60s" >&2; exit 1; fi
      sleep 1
    done
    # With ext-pcntl (in the image) the worker handles SIGTERM itself: it
    # finishes the delivery in hand, then exits. One that is cut anyway is
    # retried after retry_after.
    exec php artisan queue:work --sleep=1 --max-time=3600 --no-interaction
    ;;
  *)
    exec "$@"
    ;;
esac
