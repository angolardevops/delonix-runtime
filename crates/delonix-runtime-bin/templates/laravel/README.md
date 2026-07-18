# __NAME__ (Laravel)

A Laravel project scaffolded by `delonix init --template laravel` — served by
**FrankenPHP** (a single production-grade binary, no nginx/php-fpm to wire up),
health probes on the stateless `api` group, and a Delonix manifest. Uses
**SQLite** by default so it runs out of the box (the DB file is created at build
time); swap `DB_CONNECTION` for `pgsql`/`mysql` when you outgrow it.

## Run it locally (composer + artisan)

```bash
composer install
cp .env.example .env && php artisan key:generate
touch database/database.sqlite
php artisan serve --port __PORT__
curl localhost:__PORT__/api/v1/health/live
```

## Test

```bash
php artisan test         # runs tests/Feature/HealthTest.php against an in-memory DB
```

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```

The image bakes an `APP_KEY` and the SQLite DB. Config is **not** cached at
build (so runtime env overrides still take effect). For real secrets in
production, set `APP_KEY` (and DB credentials) via `delonix secret` and add the
corresponding `env` entries to `delonix-manifest.yaml`.
