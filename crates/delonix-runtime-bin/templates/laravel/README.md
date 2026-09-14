# __NAME__ (Laravel __TEMPLATE_VERSION__)

A Laravel project scaffolded by `delonix init -t laravel -v __TEMPLATE_VERSION__`
— pinned to `laravel/framework: ^__TEMPLATE_VERSION__`, served by
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
composer require <package>       # add a dependency
```

## Choosing a Laravel version

Regenerate with `-v <major>` or `-v <major.minor>` to pin another framework
line, e.g. `delonix init -t laravel -v 12.5 --force .` (`--force`: `init`
never overwrites without it). Omit `-v` and it falls back to this template's
own default (`__TEMPLATE_VERSION__`, from `template.meta`). The PHP version
(`composer.json`'s `"php"` constraint, the FrankenPHP base image) is
independent of `-v` — it tracks what FrankenPHP ships, not the framework line.

Pinning to an **older major** can make `composer install` refuse outright —
Composer blocks a whole `^<major>` range by default the moment ANY release in
it carries a disclosed security advisory, which by the time a major goes out
of support is close to guaranteed (verified live: `-v 11` and `-v 10` both
refuse this way as of this writing). That is Composer protecting you, not a
delonix bug; a specific `major.minor` at or above the last patched release in
that line installs fine, and `composer install --no-interaction` says exactly
which advisory IDs are blocking it if you need to check.

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

## Contributing

CI (`.github/workflows/ci.yml` for GitHub, `.gitlab-ci.yml` for GitLab) runs
`php artisan test` (SQLite, same as above) on every push and PR.
`sonar-project.properties` is ready for a SonarQube/SonarCloud scan. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the branching model (git-flow), commit
message format (Conventional Commits) and versioning (SemVer).
