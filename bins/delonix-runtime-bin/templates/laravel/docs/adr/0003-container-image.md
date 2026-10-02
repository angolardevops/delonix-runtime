# 0003 — FrankenPHP in classic mode, an unprivileged user, configuration cached at start

Status: accepted (template default)

## Context
A PHP service needs a web server in front of PHP. The image must carry no
secret, and Laravel's `config:cache` freezes whatever environment it sees.

## Decision
- Two stages: `composer:2` installs dependencies (from `composer.lock` when
  the context has one), `dunglas/frankenphp:1-php8.4` runs the application —
  one process serving HTTP and PHP, configured by the `Caddyfile`.
- Classic mode (one PHP execution per request), not worker mode.
- No `.env`, no `APP_KEY`, no `config:cache` at build. The entrypoint
  validates the configuration (`app:check-config`, exit 1 on any problem),
  caches it into `/tmp`, migrates, then starts the server. The same image
  runs the queue worker (`command: ["worker"]`).
- The unprivileged user `app` (uid 10001), created in the image, runs both
  containers. It owns `/var/lib/app` — the database volume takes that owner
  on its first mount — and nothing else. Read-only root filesystem, tmpfs for
  `/tmp`, `/data`, `/config`.
- `ext-pcntl` is compiled in (`install-php-extensions pcntl`), so the queue
  worker finishes the job in hand on SIGTERM and enforces a job's `$timeout`.

## Alternatives
- **nginx + php-fpm**: two processes and a supervisor, or two containers.
- **Worker mode (Laravel Octane)**: several times the throughput, at the
  price of state that survives between requests; adopt it deliberately, with
  `laravel/octane`, not by default.
- **`config:cache` at build**: bakes the build machine's environment into a
  layer, so runtime variables — a secret `APP_KEY` among them — are ignored.
- **uid 0 inside the container**: needs no subordinate uid range on the host
  (`/etc/subuid`; the Delonix installer sets one up), but the process could
  rewrite its own code and `/etc`. A host without a range cannot run a second
  user; there, remove `user:` from both containers of the manifest.

## Trade-off
Each request boots the framework (a few milliseconds with opcache), and
start-up runs three artisan commands before listening. The engine applies an
image's `USER` only when the manifest names the user, so `USER` in the
`Delonixfile` and `user:` in `delonix-manifest.yaml` must agree. Compiling
`pcntl` adds the PHP source unpack and a compiler run to every uncached build.
